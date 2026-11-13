//! Access to the S3 bucket that stores the published files.

use std::path::Path;

use aws_sdk_s3::{
    config::http::HttpResponse,
    error::{DisplayErrorContext, SdkError},
    primitives::{ByteStream, DateTime, DateTimeFormat},
};
use tracing::error;

use crate::{
    error::{BoxError, FetchError, StorageError},
    hash::{Hasher, Sha256},
};

/// A file that was uploaded to the bucket.
pub struct UploadedFile {
    pub object_key: String,
    pub sha256: Sha256,
}

/// An object fetched from the bucket, ready to be passed on to a client.
pub struct Object {
    pub content_length: Option<i64>,
    pub content_type: Option<String>,
    pub etag: Option<String>,
    /// Last modification time formatted as HTTP date.
    pub last_modified: Option<String>,
    /// The content of the object. This is `None` for `HEAD` requests.
    pub body: Option<ByteStream>,
}

/// The result of fetching an object from the bucket.
pub enum Fetched {
    /// The object exists and the client needs its content.
    Found(Object),
    /// The client's cached copy is still current (HTTP 304).
    NotModified {
        etag: Option<String>,
        last_modified: Option<String>,
    },
}

/// How much of an object to fetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchMethod {
    /// Metadata and content (HTTP `GET`).
    Get,
    /// Only the metadata (HTTP `HEAD`).
    Head,
}

/// Conditional request headers from the client that we pass on to S3.
///
/// These are the raw header values.
#[derive(Debug, Default)]
pub struct Conditions {
    pub if_none_match: Option<String>,
    pub if_modified_since: Option<String>,
}

/// Parse an HTTP date.
///
/// Dates that can't be parsed are ignored, which makes the request
/// unconditional, as required by RFC 9110.
fn parse_http_date(date: &str) -> Option<DateTime> {
    DateTime::from_str(date, DateTimeFormat::HttpDate).ok()
}

fn format_http_date(date: DateTime) -> Option<String> {
    date.fmt(DateTimeFormat::HttpDate).ok()
}

/// S3's answer to a conditional request whose condition failed.
const HTTP_NOT_MODIFIED: u16 = 304;

/// Turn a failed S3 request into a "not modified" answer or a fetch error.
fn handle_fetch_error<E>(
    err: SdkError<E, HttpResponse>,
    is_not_found: impl FnOnce(&E) -> bool,
    object_key: &str,
) -> Result<Fetched, FetchError>
where
    E: std::error::Error + 'static,
{
    // The SDK treats S3's 304 answer to a conditional request as an error.
    if let Some(raw) = err
        .raw_response()
        .filter(|r| r.status().as_u16() == HTTP_NOT_MODIFIED)
    {
        let header = |name: &str| raw.headers().get(name).map(str::to_owned);

        return Ok(Fetched::NotModified {
            etag: header("etag"),
            last_modified: header("last-modified"),
        });
    }

    if err.as_service_error().is_some_and(is_not_found) {
        return Err(FetchError::NoSuchObject {
            object_key: object_key.to_owned(),
        });
    }

    error!(
        "Failed to fetch {object_key:?} from bucket: {}",
        DisplayErrorContext(&err)
    );
    Err(FetchError::BackendFailure {
        object_key: object_key.to_owned(),
    })
}

pub struct Client {
    client: aws_sdk_s3::Client,
    bucket: String,
}

impl Client {
    /// Open an S3 client with configuration from the environment.
    pub async fn new_from_env(bucket: &str) -> Client {
        let amzn_config = aws_config::load_from_env().await;
        let s3_config = aws_sdk_s3::config::Builder::from(&amzn_config)
            // TODO For minio compat. Should this be configurable?
            .force_path_style(true)
            .build();

        Self {
            client: aws_sdk_s3::Client::from_conf(s3_config),
            bucket: bucket.to_owned(),
        }
    }

    /// Read a file from S3 into memory. This should only be used for
    /// small files.
    pub async fn read_file(&self, object_key: &str) -> Result<Vec<u8>, StorageError> {
        let read_error = |source: BoxError| StorageError::Read {
            object_key: object_key.to_owned(),
            source,
        };

        let response = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(object_key)
            .send()
            .await
            .map_err(|e| read_error(e.into()))?;

        let data = response
            .body
            .collect()
            .await
            .map_err(|e| read_error(e.into()))?;

        Ok(data.to_vec())
    }

    /// Fetch an object from the bucket so it can be served to a client.
    ///
    /// For [`FetchMethod::Head`], only the metadata is fetched and the returned
    /// object has no body. The `conditions` are passed on to S3, which
    /// decides whether the client's cached copy is still current.
    ///
    /// A missing object is only reported as [`FetchError::NoSuchObject`] if
    /// the credentials allow `s3:ListBucket`. Otherwise, S3 answers with
    /// "access denied", which we report as a backend failure.
    pub async fn fetch_object(
        &self,
        method: FetchMethod,
        object_key: &str,
        conditions: Conditions,
    ) -> Result<Fetched, FetchError> {
        let if_modified_since = conditions
            .if_modified_since
            .as_deref()
            .and_then(parse_http_date);

        match method {
            FetchMethod::Get => {
                let result = self
                    .client
                    .get_object()
                    .bucket(&self.bucket)
                    .key(object_key)
                    .set_if_none_match(conditions.if_none_match)
                    .set_if_modified_since(if_modified_since)
                    .send()
                    .await;

                match result {
                    Ok(response) => Ok(Fetched::Found(Object {
                        content_length: response.content_length,
                        content_type: response.content_type,
                        etag: response.e_tag,
                        last_modified: response.last_modified.and_then(format_http_date),
                        body: Some(response.body),
                    })),
                    Err(e) => handle_fetch_error(e, |se| se.is_no_such_key(), object_key),
                }
            }
            FetchMethod::Head => {
                let result = self
                    .client
                    .head_object()
                    .bucket(&self.bucket)
                    .key(object_key)
                    .set_if_none_match(conditions.if_none_match)
                    .set_if_modified_since(if_modified_since)
                    .send()
                    .await;

                match result {
                    Ok(response) => Ok(Fetched::Found(Object {
                        content_length: response.content_length,
                        content_type: response.content_type,
                        etag: response.e_tag,
                        last_modified: response.last_modified.and_then(format_http_date),
                        body: None,
                    })),
                    Err(e) => handle_fetch_error(e, |se| se.is_not_found(), object_key),
                }
            }
        }
    }

    /// Download an object and compute its SHA-256 hash.
    ///
    /// The object is streamed, so this also works for large files.
    pub async fn hash_object(&self, object_key: &str) -> Result<Sha256, StorageError> {
        let read_error = |source: BoxError| StorageError::Read {
            object_key: object_key.to_owned(),
            source,
        };

        let mut response = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(object_key)
            .send()
            .await
            .map_err(|e| read_error(e.into()))?;

        let mut hasher = Hasher::default();
        while let Some(chunk) = response
            .body
            .try_next()
            .await
            .map_err(|e| read_error(e.into()))?
        {
            hasher.update(&chunk);
        }

        Ok(hasher.finalize())
    }

    /// Upload data to the persistent store. Doesn't update any channel.
    async fn write_data(&self, object_key: &str, data: Vec<u8>) -> Result<(), StorageError> {
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(object_key)
            .body(data.into())
            .send()
            .await
            .map_err(|e| StorageError::Upload {
                object_key: object_key.to_owned(),
                source: e.into(),
            })?;

        Ok(())
    }

    /// Check whether an object exists in the bucket.
    pub async fn file_exists(&self, object_key: &str) -> Result<bool, StorageError> {
        match self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(object_key)
            .send()
            .await
        {
            Ok(_) => Ok(true),
            Err(err) if err.as_service_error().is_some_and(|e| e.is_not_found()) => Ok(false),
            Err(err) => Err(StorageError::ExistenceCheck {
                object_key: object_key.to_owned(),
                source: err.into(),
            }),
        }
    }

    /// Upload a file to the bucket, using its file name as object key.
    ///
    /// Refuses to overwrite existing objects. The file is hashed before it is
    /// uploaded.
    pub async fn upload_new_file(&self, file: &Path) -> Result<UploadedFile, StorageError> {
        let object_key = file
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| StorageError::InvalidFileName {
                path: file.to_owned(),
            })?
            .to_owned();

        if self.file_exists(&object_key).await? {
            return Err(StorageError::AlreadyExists { object_key });
        }

        // We would want to stream the file and not load it all in
        // memory, but it results in XAmzContentSHA256Mismatch. :(
        let data = tokio::fs::read(file)
            .await
            .map_err(|source| StorageError::LocalFile {
                path: file.to_owned(),
                source,
            })?;

        let mut hasher = Hasher::default();
        hasher.update(&data);
        let sha256 = hasher.finalize();

        self.write_data(&object_key, data).await?;

        Ok(UploadedFile { object_key, sha256 })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_http_date_works() {
        assert_eq!(
            parse_http_date("Wed, 21 Oct 2015 07:28:00 GMT"),
            Some(DateTime::from_secs(1445412480))
        );

        // Unparseable dates make the request unconditional.
        assert_eq!(parse_http_date("yesterday"), None);
    }
}
