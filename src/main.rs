use std::sync::Arc;
use std::{env, process};

use axum::response::{IntoResponse, Response};
use db::Db;
use http::StatusCode;
use tokio::net::TcpListener;
use tower_http::trace::TraceLayer;
use tracing::{debug, error, info};
use tracing_subscriber::{EnvFilter, FmtSubscriber};

use crate::alarm::AlarmChecksumCache;

mod alarm;
mod api;
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
    db: Db,
}

impl State {
    async fn new() -> Result<Self, sqlx::Error> {
        let alarm_checksum_cache = AlarmChecksumCache::new().await;
        let db = Db::new().await?;
        Ok(Self { alarm_checksum_cache, db })
    }
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("{0}")]
    Sql(#[from] sqlx::Error),
    #[error("invalid device {0:?}")]
    InvalidDevice(String),
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
        }
    }
}
