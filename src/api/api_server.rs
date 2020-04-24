use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use futures::future::Future;
use futures::stream::*;
use hyper::body::{Bytes, HttpBody};
use hyper::server::conn::AddrStream;
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Method, Request, Response, Server, StatusCode};

use garage_util::data::*;
use garage_util::error::Error;

use garage_table::EmptyKey;

use garage_core::block::INLINE_THRESHOLD;
use garage_core::block_ref_table::*;
use garage_core::garage::Garage;
use garage_core::object_table::*;
use garage_core::version_table::*;

use crate::http_util::*;
use crate::signature::check_signature;

type BodyType = Box<dyn HttpBody<Data = Bytes, Error = Error> + Send + Unpin>;

pub async fn run_api_server(
	garage: Arc<Garage>,
	shutdown_signal: impl Future<Output = ()>,
) -> Result<(), Error> {
	let addr = &garage.config.s3_api.api_bind_addr;

	let service = make_service_fn(|conn: &AddrStream| {
		let garage = garage.clone();
		let client_addr = conn.remote_addr();
		async move {
			Ok::<_, Error>(service_fn(move |req: Request<Body>| {
				let garage = garage.clone();
				handler(garage, req, client_addr)
			}))
		}
	});

	let server = Server::bind(&addr).serve(service);

	let graceful = server.with_graceful_shutdown(shutdown_signal);
	info!("API server listening on http://{}", addr);

	graceful.await?;
	Ok(())
}

async fn handler(
	garage: Arc<Garage>,
	req: Request<Body>,
	addr: SocketAddr,
) -> Result<Response<BodyType>, Error> {
	info!("{} {} {}", addr, req.method(), req.uri());
	debug!("{:?}", req);
	match handler_inner(garage, req).await {
		Ok(x) => {
			debug!("{} {:?}", x.status(), x.headers());
			Ok(x)
		}
		Err(e) => {
			let body: BodyType = Box::new(BytesBody::from(format!("{}\n", e)));
			let mut http_error = Response::new(body);
			*http_error.status_mut() = e.http_status_code();
			warn!("Response: error {}, {}", e.http_status_code(), e);
			Ok(http_error)
		}
	}
}

async fn handler_inner(
	garage: Arc<Garage>,
	req: Request<Body>,
) -> Result<Response<BodyType>, Error> {
	let path = req.uri().path().to_string();
	let path = path.trim_start_matches('/');
	let (bucket, key) = match path.find('/') {
		Some(i) => {
			let (bucket, key) = path.split_at(i);
			(bucket, Some(key))
		}
		None => (path, None),
	};
	if bucket.len() == 0 {
		return Err(Error::Forbidden(format!(
			"Operations on buckets not allowed"
		)));
	}

	let api_key = check_signature(&garage, &req).await?;
	let allowed = match req.method() {
		&Method::HEAD | &Method::GET => api_key.allow_read(&bucket),
		_ => api_key.allow_write(&bucket),
	};
	if !allowed {
		return Err(Error::Forbidden(format!(
			"Operation is not allowed for this key."
		)));
	}

	if let Some(key) = key {
		match req.method() {
			&Method::HEAD => Ok(handle_head(garage, &bucket, &key).await?),
			&Method::GET => Ok(handle_get(garage, &bucket, &key).await?),
			&Method::PUT => {
				let mime_type = req
					.headers()
					.get(hyper::header::CONTENT_TYPE)
					.map(|x| x.to_str())
					.unwrap_or(Ok("blob"))?
					.to_string();
				let version_uuid =
					handle_put(garage, &mime_type, &bucket, &key, req.into_body()).await?;
				let response = format!("{}\n", hex::encode(version_uuid,));
				Ok(Response::new(Box::new(BytesBody::from(response))))
			}
			&Method::DELETE => {
				let version_uuid = handle_delete(garage, &bucket, &key).await?;
				let response = format!("{}\n", hex::encode(version_uuid,));
				Ok(Response::new(Box::new(BytesBody::from(response))))
			}
			_ => Err(Error::BadRequest(format!("Invalid method"))),
		}
	} else {
		// TODO: listing buckets
		Err(Error::Forbidden("Unimplemented".into()))
	}
}

async fn handle_put(
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

async fn handle_delete(garage: Arc<Garage>, bucket: &str, key: &str) -> Result<UUID, Error> {
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

fn object_headers(version: &ObjectVersion) -> http::response::Builder {
	let date = UNIX_EPOCH + Duration::from_millis(version.timestamp);
	let date_str = httpdate::fmt_http_date(date);

	Response::builder()
		.header("Content-Type", version.mime_type.to_string())
		.header("Content-Length", format!("{}", version.size))
		.header("Last-Modified", date_str)
}

async fn handle_head(
	garage: Arc<Garage>,
	bucket: &str,
	key: &str,
) -> Result<Response<BodyType>, Error> {
	let object = match garage
		.object_table
		.get(&bucket.to_string(), &key.to_string())
		.await?
	{
		None => return Err(Error::NotFound),
		Some(o) => o,
	};

	let version = match object
		.versions()
		.iter()
		.rev()
		.filter(|v| v.is_complete && v.data != ObjectVersionData::DeleteMarker)
		.next()
	{
		Some(v) => v,
		None => return Err(Error::NotFound),
	};

	let body: BodyType = Box::new(BytesBody::from(vec![]));
	let response = object_headers(&version)
		.status(StatusCode::OK)
		.body(body)
		.unwrap();
	Ok(response)
}

async fn handle_get(
	garage: Arc<Garage>,
	bucket: &str,
	key: &str,
) -> Result<Response<BodyType>, Error> {
	let object = match garage
		.object_table
		.get(&bucket.to_string(), &key.to_string())
		.await?
	{
		None => return Err(Error::NotFound),
		Some(o) => o,
	};

	let last_v = match object
		.versions()
		.iter()
		.rev()
		.filter(|v| v.is_complete)
		.next()
	{
		Some(v) => v,
		None => return Err(Error::NotFound),
	};

	let resp_builder = object_headers(&last_v).status(StatusCode::OK);

	match &last_v.data {
		ObjectVersionData::DeleteMarker => Err(Error::NotFound),
		ObjectVersionData::Inline(bytes) => {
			let body: BodyType = Box::new(BytesBody::from(bytes.to_vec()));
			Ok(resp_builder.body(body)?)
		}
		ObjectVersionData::FirstBlock(first_block_hash) => {
			let read_first_block = garage.block_manager.rpc_get_block(&first_block_hash);
			let get_next_blocks = garage.version_table.get(&last_v.uuid, &EmptyKey);

			let (first_block, version) = futures::try_join!(read_first_block, get_next_blocks)?;
			let version = match version {
				Some(v) => v,
				None => return Err(Error::NotFound),
			};

			let mut blocks = version
				.blocks()
				.iter()
				.map(|vb| (vb.hash, None))
				.collect::<Vec<_>>();
			blocks[0].1 = Some(first_block);

			let body_stream = futures::stream::iter(blocks)
				.map(move |(hash, data_opt)| {
					let garage = garage.clone();
					async move {
						if let Some(data) = data_opt {
							Ok(Bytes::from(data))
						} else {
							garage
								.block_manager
								.rpc_get_block(&hash)
								.await
								.map(Bytes::from)
						}
					}
				})
				.buffered(2);
			let body: BodyType = Box::new(StreamBody::new(Box::pin(body_stream)));
			Ok(resp_builder.body(body)?)
		}
	}
}
