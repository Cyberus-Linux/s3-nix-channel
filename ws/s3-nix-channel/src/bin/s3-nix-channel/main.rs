mod error;

use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use axum::{
    body::Body,
    extract::{Path, Request, State},
    http::{
        header::{
            CONTENT_LENGTH, CONTENT_TYPE, ETAG, IF_MODIFIED_SINCE, IF_NONE_MATCH, LAST_MODIFIED,
            LINK,
        },
        HeaderMap, HeaderValue, Method, StatusCode,
    },
    middleware::{self, Next},
    response::{self, IntoResponse, Redirect},
    routing::get,
    Router,
};
use clap::Parser;
use entity::{alias, channel, update};
use migration::MigratorTrait;
use sea_orm::{
    ColumnTrait, ConnectionTrait, Database, DatabaseConnection, EntityTrait, JoinType, QueryFilter,
    QuerySelect, RelationTrait, Select,
};
use tower_http::trace::TraceLayer;
use tracing::{debug, info, warn};

use storage::s3::{Conditions, FetchMethod, Fetched};

use crate::error::RequestError;

/// A program to serve a S3 bucket via the Nix Lockable Tarball Protocol.
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// The S3 bucket to serve the content from.
    #[arg(long)]
    bucket: String,

    /// The base URL of the service.
    ///
    /// If you want to serve objects from
    /// https://foo.com/permanent/123.tar.xz, you need to specify
    /// https://foo.com here.
    #[arg(long)]
    base_url: String,

    /// What IP and port to listen on. Specify as <IP>:<port>, for
    /// example: 0.0.0.0:3000
    ///
    /// If this option is not specified, we'll try to get a listening
    /// socket from systemd.
    #[arg(long)]
    listen: Option<String>,

    /// Location of the database.
    ///
    /// Find more information about the format in the sea-orm manual:
    /// https://www.sea-ql.org/SeaORM/docs/install-and-config/connection/
    ///
    /// For postgres this can look like: postgres://username:password@host/database
    #[arg(long)]
    db: String,
}

struct Config {
    s3_client: storage::s3::Client,
    db: DatabaseConnection,

    base_url: String,
}

/// Find the update that `channel.latest_update_id` points to, looking up the
/// channel by any of its names.
fn latest_channel_update_query(name: &str) -> Select<update::Entity> {
    update::Entity::find()
        // JOIN channel ON channel.latest_update_id = update.id
        .join_rev(JoinType::InnerJoin, channel::Relation::Update.def())
        // JOIN alias ON alias.channel_id = channel.id
        .join(JoinType::InnerJoin, channel::Relation::Alias.def())
        .filter(alias::Column::Name.eq(name))
}

async fn latest_channel_update<C>(db: &C, name: &str) -> Result<Option<String>, RequestError>
where
    C: ConnectionTrait,
{
    let latest = latest_channel_update_query(name)
        .one(db)
        .await
        .map_err(|e| RequestError::DatabaseFailure {
            reason: e.to_string(),
        })?;

    Ok(latest.map(|u| u.persistent_file))
}

/// Redirect to the latest tarball of the requested channel.
async fn handle_channel(
    Path(path): Path<String>,
    State(config): State<Arc<Config>>,
) -> Result<impl IntoResponse, RequestError> {
    if let Some(latest_object) = latest_channel_update(&config.db, &path).await? {
        let permanent_url = format!("{}/permanent/{latest_object}", config.base_url);

        let mut headers = HeaderMap::new();

        // The Lockable HTTP Tarball Protocol. See:
        // https://nix.dev/manual/nix/2.25/protocols/tarball-fetcher
        headers.insert(
            LINK,
            HeaderValue::from_str(&format!("<{permanent_url}>; rel=\"immutable\""))
                .map_err(|_e| RequestError::Unknown)?,
        );

        // We redirect to our own permanent URL instead of a pre-signed S3 URL.
        // Tools that don't speak the Lockable HTTP Tarball Protocol (e.g. npins
        // 0.5 and later) record the final URL after redirects. Pre-signed URLs
        // expire, so they must never end up there.
        Ok((headers, Redirect::temporary(&permanent_url)))
    } else {
        Err(RequestError::NoSuchChannel { file_name: path })
    }
}

async fn log_request_middleware(req: Request, next: Next) -> response::Response {
    let path = req.uri().path().to_owned();
    let method = req.method().clone();

    let start = std::time::Instant::now();
    let response = next.run(req).await;
    let duration = start.elapsed();

    let status = response.status();

    info!(
        "Request: {} {} -> {} ({:?})",
        method,
        path,
        status.as_u16(),
        duration
    );

    response
}

/// Extract the conditional request headers that we pass on to S3.
fn conditions_from_headers(headers: &HeaderMap) -> Conditions {
    let get = |name| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };

    Conditions {
        if_none_match: get(IF_NONE_MATCH),
        if_modified_since: get(IF_MODIFIED_SINCE),
    }
}

/// Serve an object from the backing store by proxying its content.
async fn handle_persistent(
    method: Method,
    request_headers: HeaderMap,
    Path(path): Path<String>,
    State(config): State<Arc<Config>>,
) -> Result<impl IntoResponse, RequestError> {
    // TODO This is very unfortunate. We basically allow the client to grab
    // everything from the bucket here. This would include our config files as
    // well. There are no secrets in them, but it's still not great. We make a
    // half-hearted attempt at preventing it here...
    if path.ends_with(".json") {
        return Err(RequestError::NoSuchChannel { file_name: path });
    }

    let method = match method {
        Method::GET => FetchMethod::Get,
        Method::HEAD => FetchMethod::Head,
        method => return Err(RequestError::UnsupportedMethod { method }),
    };

    let fetched = config
        .s3_client
        .fetch_object(method, &path, conditions_from_headers(&request_headers))
        .await?;

    let mut headers = HeaderMap::new();
    let mut insert = |name, value: Option<String>| {
        if let Some(value) = value.and_then(|v| HeaderValue::from_str(&v).ok()) {
            headers.insert(name, value);
        }
    };

    Ok(match fetched {
        Fetched::Found(object) => {
            insert(CONTENT_TYPE, object.content_type);
            insert(CONTENT_LENGTH, object.content_length.map(|l| l.to_string()));
            insert(ETAG, object.etag);
            insert(LAST_MODIFIED, object.last_modified);

            let body = object
                .body
                .map(|stream| Body::new(stream.into_inner()))
                .unwrap_or_else(Body::empty);

            (headers, body).into_response()
        }
        Fetched::NotModified {
            etag,
            last_modified,
        } => {
            insert(ETAG, etag);
            insert(LAST_MODIFIED, last_modified);

            (StatusCode::NOT_MODIFIED, headers).into_response()
        }
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::builder().parse("info,s3-nix-channel=debug")?,
        )
        .init();

    let db = Database::connect(&args.db).await?;

    migration::Migrator::up(&db, None).await?;

    let s3_client = storage::s3::Client::new_from_env(&args.bucket).await;

    let config = Arc::new(Config {
        s3_client,
        db,
        base_url: args.base_url,
    });

    let app = Router::new()
        .route("/channel/{*path}", get(handle_channel))
        .route("/permanent/{*path}", get(handle_persistent))
        .with_state(config)
        .layer(middleware::from_fn(log_request_middleware))
        .layer(
            TraceLayer::new_for_http().make_span_with(|request: &Request| {
                tracing::debug_span!("request", method = %request.method(), uri = %request.uri())
            }),
        );

    let listener = match args.listen {
        Some(listen_str) => {
            info!("Listening on {}", &listen_str);
            tokio::net::TcpListener::bind(&listen_str).await?
        }
        None => {
            use std::os::fd::FromRawFd;
            // Accept socket from systemd.
            let socket_fd = sd_notify::listen_fds()
                .context("Failed to get listening socket from systemd")?
                .next()
                .ok_or_else(|| anyhow!("Got 0 file descriptors from systemd?"))?;
            // SAFETY: Systemd guarantees that this is an open
            // listening socket.
            let std_listener = unsafe { std::net::TcpListener::from_raw_fd(socket_fd) };

            info!("Got listening socket from systemd.");
            tokio::net::TcpListener::from_std(std_listener)?
        }
    };

    tokio::task::spawn_blocking(|| {
        use sd_notify::{notify, NotifyState};
        if let Err(e) = notify(&[NotifyState::Ready]) {
            warn!("Failed to notify systemd: {e}");
        } else {
            debug!("Notified systemd that we are ready to serve!");
        }
    })
    .await?;

    axum::serve(listener, app).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conditions_from_headers_works() {
        let mut headers = HeaderMap::new();
        let conditions = conditions_from_headers(&headers);
        assert!(conditions.if_none_match.is_none());
        assert!(conditions.if_modified_since.is_none());

        headers.insert(IF_NONE_MATCH, "\"abc\"".parse().unwrap());
        headers.insert(
            IF_MODIFIED_SINCE,
            "Wed, 21 Oct 2015 07:28:00 GMT".parse().unwrap(),
        );
        let conditions = conditions_from_headers(&headers);
        assert_eq!(conditions.if_none_match.as_deref(), Some("\"abc\""));
        assert_eq!(
            conditions.if_modified_since.as_deref(),
            Some("Wed, 21 Oct 2015 07:28:00 GMT")
        );
    }
}
