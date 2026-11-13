//! Access to the S3 bucket that stores the published files.

use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use aws_sdk_s3::{
    config::http::HttpResponse,
    error::{DisplayErrorContext, SdkError},
    primitives::{ByteStream, DateTime, DateTimeFormat},
};
use axum::{
    body::Bytes,
    http::{self, Method},
};
use tracing::error;

use crate::error::RequestError;

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

/// Conditional request headers from the client that we pass on to S3.
#[derive(Debug, Default)]
pub struct Conditions {
    pub if_none_match: Option<String>,
    pub if_modified_since: Option<DateTime>,
}

impl Conditions {
    /// Extract the conditions from the client's request headers.
    ///
    /// Headers that can't be parsed are ignored, which makes the request
    /// unconditional, as required by RFC 9110.
    pub fn from_headers(headers: &http::HeaderMap) -> Conditions {
        let get = |name| headers.get(name).and_then(|v| v.to_str().ok());

        Conditions {
            if_none_match: get(http::header::IF_NONE_MATCH).map(str::to_owned),
            if_modified_since: get(http::header::IF_MODIFIED_SINCE)
                .and_then(|s| DateTime::from_str(s, DateTimeFormat::HttpDate).ok()),
        }
    }
}

fn format_http_date(date: DateTime) -> Option<String> {
    date.fmt(DateTimeFormat::HttpDate).ok()
}

/// Turn a failed S3 request into a "not modified" answer or a request error.
fn handle_fetch_error<E>(
    err: SdkError<E, HttpResponse>,
    is_not_found: impl FnOnce(&E) -> bool,
    object_key: &str,
) -> Result<Fetched, RequestError>
where
    E: std::error::Error + 'static,
{
    // The SDK treats S3's 304 answer to a conditional request as an error.
    if let Some(raw) = err
        .raw_response()
        .filter(|r| r.status().as_u16() == http::StatusCode::NOT_MODIFIED.as_u16())
    {
        let header = |name: &str| raw.headers().get(name).map(str::to_owned);

        return Ok(Fetched::NotModified {
            etag: header("etag"),
            last_modified: header("last-modified"),
        });
    }

    if err.as_service_error().is_some_and(is_not_found) {
        return Err(RequestError::NoSuchObject {
            object_key: object_key.to_owned(),
        });
    }

    error!(
        "Failed to fetch {object_key:?} from bucket: {}",
        DisplayErrorContext(&err)
    );
    Err(RequestError::BackendFailure {
        object_key: object_key.to_owned(),
    })
}

pub struct Client {
    client: aws_sdk_s3::Client,
    bucket: String,
}

impl Client {
    /// Open an S3 client with configuration from the environment.
    // TODO Return a custom error type.
    pub async fn new_from_env(bucket: &str) -> Result<Client> {
        let amzn_config = aws_config::load_from_env().await;
        let s3_config = aws_sdk_s3::config::Builder::from(&amzn_config)
            // TODO For minio compat. Should this be configurable?
            .force_path_style(true)
            .build();

        Ok(Self {
            client: aws_sdk_s3::Client::from_conf(s3_config),
            bucket: bucket.to_owned(),
        })
    }

    /// Read a file from S3 into memory. This should only be used for
    /// small files.
    // TODO Return a custom error type.
    pub async fn read_file(&self, object_key: &str) -> Result<Bytes> {
        let response = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(object_key)
            .send()
            .await
            // TODO Better error.
            .with_context(|| format!("Failed to read: {object_key}"))?;

        Ok(response.body.collect().await?.into_bytes())
    }

    /// Fetch an object from the bucket so it can be served to a client.
    ///
    /// For `HEAD` requests, only the metadata is fetched and the returned
    /// object has no body. The `conditions` are passed on to S3, which
    /// decides whether the client's cached copy is still current.
    ///
    /// A missing object is only reported as [`RequestError::NoSuchObject`] if
    /// the credentials allow `s3:ListBucket`. Otherwise, S3 answers with
    /// "access denied", which we report as a backend failure.
    pub async fn fetch_object(
        &self,
        method: http::Method,
        object_key: &str,
        conditions: Conditions,
    ) -> Result<Fetched, RequestError> {
        match method {
            Method::GET => {
                let result = self
                    .client
                    .get_object()
                    .bucket(&self.bucket)
                    .key(object_key)
                    .set_if_none_match(conditions.if_none_match)
                    .set_if_modified_since(conditions.if_modified_since)
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
            Method::HEAD => {
                let result = self
                    .client
                    .head_object()
                    .bucket(&self.bucket)
                    .key(object_key)
                    .set_if_none_match(conditions.if_none_match)
                    .set_if_modified_since(conditions.if_modified_since)
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
            unsupported => Err(RequestError::UnsupportedMethod {
                method: unsupported,
            }),
        }
    }

    /// Upload a file to the persistent store. Doesn't update any channel.
    async fn write_file(&self, object_key: &str, file: &Path) -> Result<()> {
        // We would want to stream the file and not load it all in
        // memory, but it results in XAmzContentSHA256Mismatch. :(
        let data = tokio::fs::read(file)
            .await
            .context("Failed to read input file")?;

        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(object_key)
            .body(data.into())
            .send()
            .await
            .with_context(|| format!("Failed to upload file: {}", file.display()))?;

        Ok(())
    }

    /// Check whether an object exists in the bucket.
    pub async fn file_exists(&self, object_key: &str) -> Result<bool> {
        match self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(object_key)
            .send()
            .await
        {
            Ok(_) => Ok(true),
            Err(err) => {
                if err.as_service_error().map(|e| e.is_not_found()) == Some(true) {
                    Ok(false)
                } else {
                    Err(anyhow!("Failed to check if object exists: {err}"))
                }
            }
        }
    }

    /// Upload a file to the bucket, using its file name as object key.
    ///
    /// Refuses to overwrite existing objects. Returns the object key.
    pub async fn upload_new_file(&self, file: &Path) -> Result<String> {
        let object_key = file
            .file_name()
            .ok_or_else(|| anyhow!("No file name: {}", file.display()))?
            .to_str()
            .ok_or_else(|| anyhow!("File name needs to be valid UTF-8: {}", file.display()))?
            .to_owned();

        if self.file_exists(&object_key).await? {
            bail!("Refusing to overwrite key: {object_key}");
        }

        self.write_file(&object_key, file).await?;

        Ok(object_key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conditions_from_headers() {
        let mut headers = http::HeaderMap::new();
        assert!(Conditions::from_headers(&headers).if_none_match.is_none());
        assert!(Conditions::from_headers(&headers)
            .if_modified_since
            .is_none());

        headers.insert(http::header::IF_NONE_MATCH, "\"abc\"".parse().unwrap());
        headers.insert(
            http::header::IF_MODIFIED_SINCE,
            "Wed, 21 Oct 2015 07:28:00 GMT".parse().unwrap(),
        );
        let conditions = Conditions::from_headers(&headers);
        assert_eq!(conditions.if_none_match.as_deref(), Some("\"abc\""));
        assert_eq!(
            conditions.if_modified_since,
            Some(DateTime::from_secs(1445412480))
        );

        // Unparseable dates make the request unconditional.
        headers.insert(
            http::header::IF_MODIFIED_SINCE,
            "yesterday".parse().unwrap(),
        );
        assert!(Conditions::from_headers(&headers)
            .if_modified_since
            .is_none());
    }
}
