use std::sync::Arc;
use std::{env, io, process};

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use db::Db;
use tokio::net::TcpListener;
use tower_http::trace::TraceLayer;
use tracing::{debug, error, info};
use tracing_subscriber::{EnvFilter, FmtSubscriber};

use crate::cache::{AlarmChecksumCache, ImageCache};

mod api;
mod cache;
mod db;

#[tokio::main]
async fn main() {
    // Setup logging.
    let directives = env::var("RUST_LOG")
        .unwrap_or("warn,isotopia=debug,tower_http=debug,axum::rejection=trace".into());
    let env_filter = EnvFilter::builder().parse_lossy(directives);
    FmtSubscriber::builder().with_env_filter(env_filter).with_line_number(true).init();

    // Create server state.
    let state = match State::new().await {
        Ok(state) => Arc::new(state),
        Err(err) => {
            error!("State creation failed: {err}");
            process::exit(1);
        },
    };

    // Create our API router.
    let api = api::router().layer(TraceLayer::new_for_http()).with_state(state);

    // Bind server to TCP port.
    let listener = match TcpListener::bind("127.0.0.1:3000").await {
        Ok(listener) => listener,
        Err(err) => {
            error!("Failed to spawn server: {err}");
            process::exit(1);
        },
    };

    info!("Listening on {}", listener.local_addr().unwrap());

    // Serve API requests.
    if let Err(err) = axum::serve(listener, api).await {
        error!("Server error: {err}");
        process::exit(1);
    }
}

/// Server state.
pub struct State {
    alarm_checksum_cache: AlarmChecksumCache,
    image_cache: ImageCache,
    upload_bearer: String,
    db: Arc<Db>,
}

impl State {
    async fn new() -> Result<Self, Error> {
        let upload_secret = env::var("UPLOAD_SECRET").map_err(|_| Error::MissingUploadSecret)?;
        let upload_bearer = format!("Bearer {upload_secret}");

        let db = Arc::new(Db::new().await?);
        let alarm_checksum_cache = AlarmChecksumCache::new(db.clone()).await;
        let image_cache = ImageCache::new(db.clone()).await?;

        Ok(Self { alarm_checksum_cache, upload_bearer, image_cache, db })
    }
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("{0}")]
    Rustix(#[from] rustix::io::Errno),
    #[error("{0}")]
    Sql(#[from] sqlx::Error),
    #[error("{0}")]
    Io(#[from] io::Error),
    #[error("invalid device {0:?}")]
    InvalidDevice(String),
    #[error("missing UPLOAD_SECRET environment variable")]
    MissingUploadSecret,
    #[error("invalid request status transition")]
    StatusConflict,
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        debug!("Request failed: {}", self);

        match self {
            Self::Sql(_) => StatusCode::OK.into_response(),
            Self::StatusConflict => StatusCode::CONFLICT.into_response(),
            Self::InvalidDevice(device) => {
                let msg = format!(
                    "unexpected device {device:?}; expected one of `pinephone`, or `pinephone-pro`"
                );
                (StatusCode::BAD_REQUEST, msg).into_response()
            },
            Self::Rustix(_) | Self::Io(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
            // This error is never returned inside a request.
            Self::MissingUploadSecret => unreachable!(),
        }
    }
}
