use std::net::SocketAddr;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IceServer {
    pub urls: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
}

/// Relay settings, from `relay.toml` and/or CLI flags.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RelayConfig {
    /// Address to listen on.
    pub listen: SocketAddr,
    /// Directory for the redb database and the relay's device key.
    pub data_dir: PathBuf,
    /// App ids this relay serves. Empty means any app id.
    pub apps: Vec<String>,
    /// STUN/TURN servers handed to browsers at `GET /v1/{app}/ice`.
    pub ice_servers: Vec<IceServer>,
    /// Max sync frames per second per connection (burst is twice this).
    pub max_frames_per_sec: u32,
    /// Max size of one WebSocket frame.
    pub max_frame_bytes: usize,
}

impl Default for RelayConfig {
    fn default() -> Self {
        RelayConfig {
            listen: "0.0.0.0:8787".parse().unwrap(),
            data_dir: "./data".into(),
            apps: vec![],
            ice_servers: vec![IceServer {
                urls: vec!["stun:stun.l.google.com:19302".into()],
                username: None,
                credential: None,
            }],
            max_frames_per_sec: 200,
            max_frame_bytes: 4 * 1024 * 1024,
        }
    }
}

impl RelayConfig {
    pub fn serves(&self, app: &str) -> bool {
        valid_app_id(app) && (self.apps.is_empty() || self.apps.iter().any(|a| a == app))
    }
}

pub fn valid_app_id(app: &str) -> bool {
    (1..=64).contains(&app.len())
        && app
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
}
