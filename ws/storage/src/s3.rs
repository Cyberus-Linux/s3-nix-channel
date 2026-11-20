//! Access to the S3 bucket that stores the published files.

use std::path::Path;

use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures::{stream::BoxStream, TryStreamExt};
use object_store::{
    aws::{AmazonS3, AmazonS3Builder},
    path::Path as ObjectPath,
    Attribute, GetOptions, ObjectStore, ObjectStoreExt, WriteMultipart,
};
use tokio::io::AsyncReadExt;
use tracing::error;

use crate::{
    error::{BoxError, FetchError, StorageError},
    hash::{Hasher, Sha256},
};

/// How much of a local file we read at once while uploading it.
const UPLOAD_CHUNK_SIZE: usize = 1 << 20;

/// How many parts of a multipart upload we send at the same time.
const UPLOAD_CONCURRENCY: usize = 4;

/// A file that was uploaded to the bucket.
pub struct UploadedFile {
    pub object_key: String,
    pub sha256: Sha256,
}

/// The content of an object, streamed from the bucket.
pub type ObjectStream = BoxStream<'static, Result<Bytes, object_store::Error>>;

/// An object fetched from the bucket, ready to be passed on to a client.
pub struct Object {
    pub content_length: u64,
    pub content_type: Option<String>,
    pub etag: Option<String>,
    /// Last modification time formatted as HTTP date.
    pub last_modified: String,
    /// The content of the object. This is `None` for `HEAD` requests.
    pub body: Option<ObjectStream>,
}

/// The result of fetching an object from the bucket.
pub enum Fetched {
    /// The object exists and the client needs its content.
    Found(Object),
    /// The client's cached copy is still current (HTTP 304).
    NotModified {
        /// The client's `If-None-Match` value, if it sent one. S3's own answer
        /// isn't available.
        etag: Option<String>,
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
fn parse_http_date(date: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc2822(date)
        .ok()
        .map(|date| date.with_timezone(&Utc))
}

fn format_http_date(date: DateTime<Utc>) -> String {
    date.format("%a, %d %b %Y %H:%M:%S GMT").to_string()
}

pub struct Client {
    store: AmazonS3,
}

impl Client {
    /// Open an S3 client with configuration from the environment.
    ///
    /// See [`AmazonS3Builder::from_env`] for the supported variables, such as
    /// `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_REGION`, and
    /// `AWS_ENDPOINT_URL`. Endpoints using plain HTTP also need
    /// `AWS_ALLOW_HTTP=true`.
    pub fn new_from_env(bucket: &str) -> Result<Client, StorageError> {
        let store = AmazonS3Builder::from_env()
            .with_bucket_name(bucket)
            .build()
            .map_err(|e| StorageError::Configuration { source: e.into() })?;

        Ok(Self { store })
    }

    /// Read a file from S3 into memory. This should only be used for
    /// small files.
    pub async fn read_file(&self, object_key: &str) -> Result<Vec<u8>, StorageError> {
        let read_error = |source: object_store::Error| StorageError::Read {
            object_key: object_key.to_owned(),
            source: source.into(),
        };

        let data = self
            .store
            .get(&ObjectPath::from(object_key))
            .await
            .map_err(read_error)?
            .bytes()
            .await
            .map_err(read_error)?;

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
        let options = GetOptions {
            if_none_match: conditions.if_none_match.clone(),
            if_modified_since: conditions
                .if_modified_since
                .as_deref()
                .and_then(parse_http_date),
            head: method == FetchMethod::Head,
            ..GetOptions::default()
        };

        match self
            .store
            .get_opts(&ObjectPath::from(object_key), options)
            .await
        {
            Ok(result) => Ok(Fetched::Found(Object {
                content_length: result.meta.size,
                content_type: result
                    .attributes
                    .get(&Attribute::ContentType)
                    .map(|value| value.to_string()),
                etag: result.meta.e_tag.clone(),
                last_modified: format_http_date(result.meta.last_modified),
                body: (method == FetchMethod::Get).then(|| result.into_stream()),
            })),
            Err(object_store::Error::NotModified { .. }) => Ok(Fetched::NotModified {
                etag: conditions.if_none_match,
            }),
            Err(object_store::Error::NotFound { .. }) => Err(FetchError::NoSuchObject {
                object_key: object_key.to_owned(),
            }),
            Err(err) => {
                error!("Failed to fetch {object_key:?} from bucket: {err}");
                Err(FetchError::BackendFailure {
                    object_key: object_key.to_owned(),
                })
            }
        }
    }

    /// Download an object and compute its SHA-256 hash.
    ///
    /// The object is streamed, so this also works for large files.
    pub async fn hash_object(&self, object_key: &str) -> Result<Sha256, StorageError> {
        let read_error = |source: object_store::Error| StorageError::Read {
            object_key: object_key.to_owned(),
            source: source.into(),
        };

        let mut stream = self
            .store
            .get(&ObjectPath::from(object_key))
            .await
            .map_err(read_error)?
            .into_stream();

        let mut hasher = Hasher::default();
        while let Some(chunk) = stream.try_next().await.map_err(read_error)? {
            hasher.update(&chunk);
        }

        Ok(hasher.finalize())
    }

    /// Check whether an object exists in the bucket.
    pub async fn file_exists(&self, object_key: &str) -> Result<bool, StorageError> {
        match self.store.head(&ObjectPath::from(object_key)).await {
            Ok(_) => Ok(true),
            Err(object_store::Error::NotFound { .. }) => Ok(false),
            Err(err) => Err(StorageError::ExistenceCheck {
                object_key: object_key.to_owned(),
                source: err.into(),
            }),
        }
    }

    /// Upload a file to the bucket, using its file name as object key.
    ///
    /// Refuses to overwrite existing objects. The file is streamed to the
    /// bucket and hashed on the way.
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

        let local_file_error = |source| StorageError::LocalFile {
            path: file.to_owned(),
            source,
        };
        let upload_error = |source: object_store::Error| StorageError::Upload {
            object_key: object_key.clone(),
            source: BoxError::from(source),
        };

        let mut local_file = tokio::fs::File::open(file)
            .await
            .map_err(local_file_error)?;
        let upload = self
            .store
            .put_multipart(&ObjectPath::from(object_key.as_str()))
            .await
            .map_err(upload_error)?;
        let mut writer = WriteMultipart::new(upload);

        let mut hasher = Hasher::default();
        let mut buffer = vec![0; UPLOAD_CHUNK_SIZE];

        let copied: Result<(), StorageError> = async {
            loop {
                let read = local_file
                    .read(&mut buffer)
                    .await
                    .map_err(local_file_error)?;
                if read == 0 {
                    return Ok(());
                }

                hasher.update(&buffer[..read]);
                writer
                    .wait_for_capacity(UPLOAD_CONCURRENCY)
                    .await
                    .map_err(upload_error)?;
                writer.write(&buffer[..read]);
            }
        }
        .await;

        match copied {
            Ok(()) => {
                writer.finish().await.map_err(upload_error)?;
            }
            Err(err) => {
                // Don't leave the incomplete upload behind in the bucket.
                if let Err(abort_err) = writer.abort().await {
                    error!("Failed to abort upload of {object_key:?}: {abort_err}");
                }
                return Err(err);
            }
        }

        Ok(UploadedFile {
            object_key,
            sha256: hasher.finalize(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_dates_work() {
        let date = DateTime::from_timestamp(1445412480, 0).unwrap();

        assert_eq!(parse_http_date("Wed, 21 Oct 2015 07:28:00 GMT"), Some(date));
        assert_eq!(format_http_date(date), "Wed, 21 Oct 2015 07:28:00 GMT");

        // Unparseable dates make the request unconditional.
        assert_eq!(parse_http_date("yesterday"), None);
    }
}
