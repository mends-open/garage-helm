use std::collections::VecDeque;
use std::sync::Arc;

use futures::stream::*;
use hyper::Body;

use garage_util::data::*;
use garage_util::error::Error;

use garage_core::block::INLINE_THRESHOLD;
use garage_core::block_ref_table::*;
use garage_core::garage::Garage;
use garage_core::object_table::*;
use garage_core::version_table::*;

pub async fn handle_put(
	garage: Arc<Garage>,
	mime_type: &str,
	bucket: &str,
	key: &str,
	body: Body,
) -> Result<UUID, Error> {
	let version_uuid = gen_uuid();

	let mut chunker = BodyChunker::new(body, garage.config.block_size);
	let first_block = match chunker.next().await? {
		Some(x) => x,
		None => return Err(Error::BadRequest(format!("Empty body"))),
	};

	let mut object_version = ObjectVersion {
		uuid: version_uuid,
		timestamp: now_msec(),
		mime_type: mime_type.to_string(),
		size: first_block.len() as u64,
		is_complete: false,
		data: ObjectVersionData::DeleteMarker,
	};

	if first_block.len() < INLINE_THRESHOLD {
		object_version.data = ObjectVersionData::Inline(first_block);
		object_version.is_complete = true;

		let object = Object::new(bucket.into(), key.into(), vec![object_version]);
		garage.object_table.insert(&object).await?;
		return Ok(version_uuid);
	}

	let version = Version::new(version_uuid, bucket.into(), key.into(), false, vec![]);

	let first_block_hash = hash(&first_block[..]);
	object_version.data = ObjectVersionData::FirstBlock(first_block_hash);
	let object = Object::new(bucket.into(), key.into(), vec![object_version.clone()]);
	garage.object_table.insert(&object).await?;

	let mut next_offset = first_block.len();
	let mut put_curr_version_block = put_block_meta(garage.clone(), &version, 0, first_block_hash);
	let mut put_curr_block = garage
		.block_manager
		.rpc_put_block(first_block_hash, first_block);

	loop {
		let (_, _, next_block) =
			futures::try_join!(put_curr_block, put_curr_version_block, chunker.next())?;
		if let Some(block) = next_block {
			let block_hash = hash(&block[..]);
			let block_len = block.len();
			put_curr_version_block =
				put_block_meta(garage.clone(), &version, next_offset as u64, block_hash);
			put_curr_block = garage.block_manager.rpc_put_block(block_hash, block);
			next_offset += block_len;
		} else {
			break;
		}
	}

	// TODO: if at any step we have an error, we should undo everything we did

	object_version.is_complete = true;
	object_version.size = next_offset as u64;

	let object = Object::new(bucket.into(), key.into(), vec![object_version]);
	garage.object_table.insert(&object).await?;

	Ok(version_uuid)
}

async fn put_block_meta(
	garage: Arc<Garage>,
	version: &Version,
	offset: u64,
	hash: Hash,
) -> Result<(), Error> {
	// TODO: don't clone, restart from empty block list ??
	let mut version = version.clone();
	version.add_block(VersionBlock { offset, hash }).unwrap();

	let block_ref = BlockRef {
		block: hash,
		version: version.uuid,
		deleted: false,
	};

	futures::try_join!(
		garage.version_table.insert(&version),
		garage.block_ref_table.insert(&block_ref),
	)?;
	Ok(())
}

struct BodyChunker {
	body: Body,
	read_all: bool,
	block_size: usize,
	buf: VecDeque<u8>,
}

impl BodyChunker {
	fn new(body: Body, block_size: usize) -> Self {
		Self {
			body,
			read_all: false,
			block_size,
			buf: VecDeque::new(),
		}
	}
	async fn next(&mut self) -> Result<Option<Vec<u8>>, Error> {
		while !self.read_all && self.buf.len() < self.block_size {
			if let Some(block) = self.body.next().await {
				let bytes = block?;
				trace!("Body next: {} bytes", bytes.len());
				self.buf.extend(&bytes[..]);
			} else {
				self.read_all = true;
			}
		}
		if self.buf.len() == 0 {
			Ok(None)
		} else if self.buf.len() <= self.block_size {
			let block = self.buf.drain(..).collect::<Vec<u8>>();
			Ok(Some(block))
		} else {
			let block = self.buf.drain(..self.block_size).collect::<Vec<u8>>();
			Ok(Some(block))
		}
	}
}

pub async fn handle_delete(garage: Arc<Garage>, bucket: &str, key: &str) -> Result<UUID, Error> {
	let exists = match garage
		.object_table
		.get(&bucket.to_string(), &key.to_string())
		.await?
	{
		None => false,
		Some(o) => {
			let mut has_active_version = false;
			for v in o.versions().iter() {
				if v.data != ObjectVersionData::DeleteMarker {
					has_active_version = true;
					break;
				}
			}
			has_active_version
		}
	};

	if !exists {
		// No need to delete
		return Ok([0u8; 32].into());
	}

	let version_uuid = gen_uuid();

	let object = Object::new(
		bucket.into(),
		key.into(),
		vec![ObjectVersion {
			uuid: version_uuid,
			timestamp: now_msec(),
			mime_type: "application/x-delete-marker".into(),
			size: 0,
			is_complete: true,
			data: ObjectVersionData::DeleteMarker,
		}],
	);

	garage.object_table.insert(&object).await?;
	return Ok(version_uuid);
}
