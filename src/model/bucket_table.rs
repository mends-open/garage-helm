use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use garage_table::crdt::CRDT;
use garage_table::*;

use garage_util::error::Error;

use crate::key_table::PermissionSet;

use model010::bucket_table as prev;

/// A bucket is a collection of objects
///
/// Its parameters are not directly accessible as:
///  - It must be possible to merge paramaters, hence the use of a LWW CRDT.
///  - A bucket has 2 states, Present or Deleted and parameters make sense only if present.
#[derive(PartialEq, Clone, Debug, Serialize, Deserialize)]
pub struct Bucket {
	// Primary key
	pub name: String,

	pub state: crdt::LWW<BucketState>,
}

#[derive(PartialEq, Clone, Debug, Serialize, Deserialize)]
pub enum BucketState {
	Deleted,
	Present(BucketParams),
}

impl CRDT for BucketState {
	fn merge(&mut self, o: &Self) {
		match o {
			BucketState::Deleted => *self = BucketState::Deleted,
			BucketState::Present(other_params) => {
				if let BucketState::Present(params) = self {
					params.merge(other_params);
				}
			}
		}
	}
}

#[derive(PartialEq, Clone, Debug, Serialize, Deserialize)]
pub struct BucketParams {
	pub authorized_keys: crdt::LWWMap<String, PermissionSet>,
	pub website: crdt::LWW<bool>
}

impl CRDT for BucketParams {
	fn merge(&mut self, o: &Self) {
		self.authorized_keys.merge(&o.authorized_keys);
		self.website.merge(&o.website);
	}
}

impl Bucket {
	pub fn new(name: String) -> Self {
		Bucket {
			name,
			state: crdt::LWW::new(BucketState::Present(BucketParams {
				authorized_keys: crdt::LWWMap::new(),
				website: crdt::LWW::new(false)
			})),
		}
	}
	pub fn is_deleted(&self) -> bool {
		*self.state.get() == BucketState::Deleted
	}
	pub fn authorized_keys(&self) -> &[(String, u64, PermissionSet)] {
		match self.state.get() {
			BucketState::Deleted => &[],
			BucketState::Present(state) => state.authorized_keys.items(),
		}
	}
}

impl Entry<EmptyKey, String> for Bucket {
	fn partition_key(&self) -> &EmptyKey {
		&EmptyKey
	}
	fn sort_key(&self) -> &String {
		&self.name
	}

	fn merge(&mut self, other: &Self) {
		self.state.merge(&other.state);
	}
}

pub struct BucketTable;

#[async_trait]
impl TableSchema for BucketTable {
	type P = EmptyKey;
	type S = String;
	type E = Bucket;
	type Filter = DeletedFilter;

	async fn updated(&self, _old: Option<Self::E>, _new: Option<Self::E>) -> Result<(), Error> {
		Ok(())
	}

	fn matches_filter(entry: &Self::E, filter: &Self::Filter) -> bool {
		filter.apply(entry.is_deleted())
	}

	fn try_migrate(bytes: &[u8]) -> Option<Self::E> {
		let old = match rmp_serde::decode::from_read_ref::<_, prev::Bucket>(bytes) {
			Ok(x) => x,
			Err(_) => return None,
		};
		if old.deleted {
			Some(Bucket {
				name: old.name,
				state: crdt::LWW::migrate_from_raw(old.timestamp, BucketState::Deleted),
			})
		} else {
			let mut keys = crdt::LWWMap::new();
			for ak in old.authorized_keys() {
				keys.merge(&crdt::LWWMap::migrate_from_raw_item(
					ak.key_id.clone(),
					ak.timestamp,
					PermissionSet {
						allow_read: ak.allow_read,
						allow_write: ak.allow_write,
					},
				));
			}
			
			let params = BucketParams {
				authorized_keys: keys,
				website: crdt::LWW::new(false)
			};

			Some(Bucket {
				name: old.name,
				state: crdt::LWW::migrate_from_raw(old.timestamp, BucketState::Present(params)),
			})
		}
	}
}
