use hyper::{Body, Request, Response};
use idna::domain_to_unicode;
use serde::{Deserialize, Serialize};

use crate::common_error::{CommonError as Error, *};

/// What kind of authorization is required to perform a given action
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Authorization {
	/// No authorization is required
	None,
	/// Having Read permission on bucket
	Read,
	/// Having Write permission on bucket
	Write,
	/// Having Owner permission on bucket
	Owner,
}

/// Host to bucket
///
/// Convert a host, like "bucket.garage-site.tld" to the corresponding bucket "bucket",
/// considering that ".garage-site.tld" is the "root domain". For domains not matching
/// the provided root domain, no bucket is returned
/// This behavior has been chosen to follow AWS S3 semantic.
pub fn host_to_bucket<'a>(host: &'a str, root: &str) -> Option<&'a str> {
	let root = root.trim_start_matches('.');
	let label_root = root.chars().filter(|c| c == &'.').count() + 1;
	let root = root.rsplit('.');
	let mut host = host.rsplitn(label_root + 1, '.');
	for root_part in root {
		let host_part = host.next()?;
		if root_part != host_part {
			return None;
		}
	}
	host.next()
}

/// Extract host from the authority section given by the HTTP host header
///
/// The HTTP host contains both a host and a port.
/// Extracting the port is more complex than just finding the colon (:) symbol due to IPv6
/// We do not use the collect pattern as there is no way in std rust to collect over a stack allocated value
/// check here: <https://docs.rs/collect_slice/1.2.0/collect_slice/>
pub fn authority_to_host(authority: &str) -> Result<String, Error> {
	let mut iter = authority.chars().enumerate();
	let (_, first_char) = iter
		.next()
		.ok_or_else(|| Error::bad_request("Authority is empty".to_string()))?;

	let split = match first_char {
		'[' => {
			let mut iter = iter.skip_while(|(_, c)| c != &']');
			match iter.next() {
				Some((_, ']')) => iter.next(),
				_ => {
					return Err(Error::bad_request(format!(
						"Authority {} has an illegal format",
						authority
					)))
				}
			}
		}
		_ => iter.find(|(_, c)| *c == ':'),
	};

	let authority = match split {
		Some((i, ':')) => Ok(&authority[..i]),
		None => Ok(authority),
		Some((_, _)) => Err(Error::bad_request(format!(
			"Authority {} has an illegal format",
			authority
		))),
	};
	authority.map(|h| domain_to_unicode(h).0)
}

/// Extract the bucket name and the key name from an HTTP path and possibly a bucket provided in
/// the host header of the request
///
/// S3 internally manages only buckets and keys. This function splits
/// an HTTP path to get the corresponding bucket name and key.
pub fn parse_bucket_key<'a>(
	path: &'a str,
	host_bucket: Option<&'a str>,
) -> Result<(&'a str, Option<&'a str>), Error> {
	let path = path.trim_start_matches('/');

	if let Some(bucket) = host_bucket {
		if !path.is_empty() {
			return Ok((bucket, Some(path)));
		} else {
			return Ok((bucket, None));
		}
	}

	let (bucket, key) = match path.find('/') {
		Some(i) => {
			let key = &path[i + 1..];
			if !key.is_empty() {
				(&path[..i], Some(key))
			} else {
				(&path[..i], None)
			}
		}
		None => (path, None),
	};
	if bucket.is_empty() {
		return Err(Error::bad_request("No bucket specified"));
	}
	Ok((bucket, key))
}

const UTF8_BEFORE_LAST_CHAR: char = '\u{10FFFE}';

/// Compute the key after the prefix
pub fn key_after_prefix(pfx: &str) -> Option<String> {
	let mut next = pfx.to_string();
	while !next.is_empty() {
		let tail = next.pop().unwrap();
		if tail >= char::MAX {
			continue;
		}

		// Circumvent a limitation of RangeFrom that overflow earlier than needed
		// See: https://doc.rust-lang.org/core/ops/struct.RangeFrom.html
		let new_tail = if tail == UTF8_BEFORE_LAST_CHAR {
			char::MAX
		} else {
			(tail..).nth(1).unwrap()
		};

		next.push(new_tail);
		return Some(next);
	}

	None
}

pub async fn parse_json_body<T: for<'de> Deserialize<'de>>(req: Request<Body>) -> Result<T, Error> {
	let body = hyper::body::to_bytes(req.into_body()).await?;
	let resp: T = serde_json::from_slice(&body).ok_or_bad_request("Invalid JSON")?;
	Ok(resp)
}

pub fn json_ok_response<T: Serialize>(res: &T) -> Result<Response<Body>, Error> {
	let resp_json = serde_json::to_string_pretty(res).map_err(garage_util::error::Error::from)?;
	Ok(Response::builder()
		.status(hyper::StatusCode::OK)
		.header(http::header::CONTENT_TYPE, "application/json")
		.body(Body::from(resp_json))?)
}

pub fn is_default<T: Default + PartialEq>(v: &T) -> bool {
	*v == T::default()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parse_bucket_containing_a_key() -> Result<(), Error> {
		let (bucket, key) = parse_bucket_key("/my_bucket/a/super/file.jpg", None)?;
		assert_eq!(bucket, "my_bucket");
		assert_eq!(key.expect("key must be set"), "a/super/file.jpg");
		Ok(())
	}

	#[test]
	fn parse_bucket_containing_no_key() -> Result<(), Error> {
		let (bucket, key) = parse_bucket_key("/my_bucket/", None)?;
		assert_eq!(bucket, "my_bucket");
		assert!(key.is_none());
		let (bucket, key) = parse_bucket_key("/my_bucket", None)?;
		assert_eq!(bucket, "my_bucket");
		assert!(key.is_none());
		Ok(())
	}

	#[test]
	fn parse_bucket_containing_no_bucket() {
		let parsed = parse_bucket_key("", None);
		assert!(parsed.is_err());
		let parsed = parse_bucket_key("/", None);
		assert!(parsed.is_err());
		let parsed = parse_bucket_key("////", None);
		assert!(parsed.is_err());
	}

	#[test]
	fn parse_bucket_with_vhost_and_key() -> Result<(), Error> {
		let (bucket, key) = parse_bucket_key("/a/super/file.jpg", Some("my-bucket"))?;
		assert_eq!(bucket, "my-bucket");
		assert_eq!(key.expect("key must be set"), "a/super/file.jpg");
		Ok(())
	}

	#[test]
	fn parse_bucket_with_vhost_no_key() -> Result<(), Error> {
		let (bucket, key) = parse_bucket_key("", Some("my-bucket"))?;
		assert_eq!(bucket, "my-bucket");
		assert!(key.is_none());
		let (bucket, key) = parse_bucket_key("/", Some("my-bucket"))?;
		assert_eq!(bucket, "my-bucket");
		assert!(key.is_none());
		Ok(())
	}

	#[test]
	fn authority_to_host_with_port() -> Result<(), Error> {
		let domain = authority_to_host("[::1]:3902")?;
		assert_eq!(domain, "[::1]");
		let domain2 = authority_to_host("garage.tld:65200")?;
		assert_eq!(domain2, "garage.tld");
		let domain3 = authority_to_host("127.0.0.1:80")?;
		assert_eq!(domain3, "127.0.0.1");
		Ok(())
	}

	#[test]
	fn authority_to_host_without_port() -> Result<(), Error> {
		let domain = authority_to_host("[::1]")?;
		assert_eq!(domain, "[::1]");
		let domain2 = authority_to_host("garage.tld")?;
		assert_eq!(domain2, "garage.tld");
		let domain3 = authority_to_host("127.0.0.1")?;
		assert_eq!(domain3, "127.0.0.1");
		assert!(authority_to_host("[").is_err());
		assert!(authority_to_host("[hello").is_err());
		Ok(())
	}

	#[test]
	fn host_to_bucket_test() {
		assert_eq!(
			host_to_bucket("john.doe.garage.tld", ".garage.tld").unwrap(),
			"john.doe"
		);

		assert_eq!(
			host_to_bucket("john.doe.garage.tld", "garage.tld").unwrap(),
			"john.doe"
		);

		assert_eq!(host_to_bucket("john.doe.com", "garage.tld"), None);

		assert_eq!(host_to_bucket("john.doe.com", ".garage.tld"), None);

		assert_eq!(host_to_bucket("garage.tld", "garage.tld"), None);

		assert_eq!(host_to_bucket("garage.tld", ".garage.tld"), None);

		assert_eq!(host_to_bucket("not-garage.tld", "garage.tld"), None);
		assert_eq!(host_to_bucket("not-garage.tld", ".garage.tld"), None);
	}

	#[test]
	fn test_key_after_prefix() {
		use std::iter::FromIterator;

		assert_eq!(UTF8_BEFORE_LAST_CHAR as u32, (char::MAX as u32) - 1);
		assert_eq!(key_after_prefix("a/b/").unwrap().as_str(), "a/b0");
		assert_eq!(key_after_prefix("€").unwrap().as_str(), "₭");
		assert_eq!(
			key_after_prefix("􏿽").unwrap().as_str(),
			String::from(char::from_u32(0x10FFFE).unwrap())
		);

		// When the last character is the biggest UTF8 char
		let a = String::from_iter(['a', char::MAX].iter());
		assert_eq!(key_after_prefix(a.as_str()).unwrap().as_str(), "b");

		// When all characters are the biggest UTF8 char
		let b = String::from_iter([char::MAX; 3].iter());
		assert!(key_after_prefix(b.as_str()).is_none());

		// Check utf8 surrogates
		let c = String::from('\u{D7FF}');
		assert_eq!(
			key_after_prefix(c.as_str()).unwrap().as_str(),
			String::from('\u{E000}')
		);

		// Check the character before the biggest one
		let d = String::from('\u{10FFFE}');
		assert_eq!(
			key_after_prefix(d.as_str()).unwrap().as_str(),
			String::from(char::MAX)
		);
	}
}

#[derive(Serialize)]
pub(crate) struct CustomApiErrorBody {
	pub(crate) code: String,
	pub(crate) message: String,
	pub(crate) region: String,
	pub(crate) path: String,
}
