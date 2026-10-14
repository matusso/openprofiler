//! `ember-daemon`: the single query server behind the desktop UI, the web UI, the CLI and MCP.
//!
//! - [`Engine`] manages sessions (files and live JVMs) and is usable in-process.
//! - [`query`] evaluates Query API v1 requests (docs/spec/query-api.md) against a session.
//! - [`http`] exposes the Query API over HTTP + WebSocket; [`start`] runs it.
//! - [`mcp`] is the Model Context Protocol server built on the same queries.

pub mod demo;
pub mod engine;
pub mod events;
pub mod export;
pub mod heap;
pub mod http;
pub mod ide;
pub mod insights;
pub mod live;
pub mod loaders;
pub mod mcp;
pub mod query;
pub mod save;

pub use engine::{Engine, Session, SessionKind, SessionState};

use anyhow::Result;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct DaemonConfig {
    pub bind: SocketAddr,
    /// API token; a random one is generated when `None`.
    pub token: Option<String>,
    /// Directory with the built web UI (`ui/dist`), served at `/` in web mode.
    pub ui_dir: Option<PathBuf>,
    /// Web mode: serve the UI. Requires `ui_dir`.
    pub web: bool,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self { bind: SocketAddr::from(([127, 0, 0, 1], 0)), token: None, ui_dir: None, web: false }
    }
}

#[derive(Clone)]
pub struct RunningDaemon {
    pub addr: SocketAddr,
    pub token: String,
    pub engine: Arc<Engine>,
}

impl RunningDaemon {
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

pub fn random_token() -> String {
    use rand::RngCore;
    let mut b = [0u8; 24];
    rand::rng().fill_bytes(&mut b);
    hex::encode(b)
}

/// Binds and serves the Query API in the background. Returns once the socket is listening.
pub async fn start(cfg: DaemonConfig) -> Result<RunningDaemon> {
    start_with_engine(cfg, Engine::new()).await
}

pub async fn start_with_engine(cfg: DaemonConfig, engine: Arc<Engine>) -> Result<RunningDaemon> {
    let token = cfg.token.clone().unwrap_or_else(random_token);
    let listener = tokio::net::TcpListener::bind(cfg.bind).await?;
    let addr = listener.local_addr()?;
    let state = http::AppState {
        engine: engine.clone(),
        token: token.clone().into(),
        ui_dir: if cfg.web { cfg.ui_dir.clone().map(Arc::new) } else { None },
    };
    let app = http::router(state);
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!("daemon stopped: {e}");
        }
    });
    Ok(RunningDaemon { addr, token, engine })
}
