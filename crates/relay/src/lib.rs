//! aghanim-network relay.
//!
//! A relay is an ordinary aghanim peer that runs natively. It:
//! - forwards WebRTC signaling between browsers of the same app;
//! - stores every scope its clients sync, in redb, so data outlives the
//!   browsers that wrote it;
//! - syncs over the WebSocket itself when WebRTC can't connect.
//!
//! It verifies every change like any other peer, and only ever holds
//! ciphertext for `spaces/…` content.

pub mod config;
pub mod kv;
pub mod server;

use std::sync::Arc;

pub use config::{IceServer, RelayConfig};
pub use server::{router, Relay};

/// Serves the relay on an already-bound listener until `shutdown` resolves.
pub async fn serve(
    listener: tokio::net::TcpListener,
    relay: Arc<Relay>,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    axum::serve(listener, router(relay))
        .with_graceful_shutdown(shutdown)
        .await
}
