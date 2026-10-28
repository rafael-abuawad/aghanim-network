//! WebSocket server: one sync node per app id, plus signaling rooms.
//!
//! Each WebSocket connection carries two things:
//! - **binary frames:** the sync protocol between the client and the relay's
//!   own node (the relay stores every scope its clients use, and is the
//!   fallback path when WebRTC can't connect);
//! - **text frames:** JSON signaling for WebRTC. A client may only `join`
//!   after the sync handshake, under the device key it proved it holds, so
//!   peers can't impersonate each other in signaling.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use aghanim_sync::{Config, ConnId, ConnOpts, Effect, Node};
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use redb::Database;
use serde_json::{json, Value as JsonValue};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::config::RelayConfig;
use crate::kv::RedbKv;

pub struct Relay {
    pub config: RelayConfig,
    db: Arc<Database>,
    hubs: Mutex<HashMap<String, Arc<Mutex<Hub>>>>,
    next_conn: AtomicU32,
}

struct ConnHandle {
    tx: mpsc::UnboundedSender<Message>,
    peer: Option<String>,
}

pub struct Hub {
    pub node: Node<RedbKv>,
    conns: HashMap<ConnId, ConnHandle>,
    /// Signaling room: device key (hex) -> connection.
    joined: HashMap<String, ConnId>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl Relay {
    pub fn open(config: RelayConfig) -> std::io::Result<Arc<Relay>> {
        std::fs::create_dir_all(&config.data_dir)?;
        let db =
            Database::create(config.data_dir.join("relay.redb")).map_err(std::io::Error::other)?;
        Ok(Arc::new(Relay {
            config,
            db: Arc::new(db),
            hubs: Mutex::new(HashMap::new()),
            next_conn: AtomicU32::new(1),
        }))
    }

    pub fn hub(&self, app: &str) -> Arc<Mutex<Hub>> {
        let mut hubs = self.hubs.lock().unwrap();
        hubs.entry(app.to_owned())
            .or_insert_with(|| {
                let mut cfg = Config::new(app);
                cfg.replicate_all = true;
                let node = Node::open(RedbKv::new(self.db.clone(), format!("app:{app}")), cfg);
                info!(app, device = hex::encode(node.device_id()), "opened app");
                Arc::new(Mutex::new(Hub {
                    node,
                    conns: HashMap::new(),
                    joined: HashMap::new(),
                }))
            })
            .clone()
    }
}

impl Hub {
    fn send(&self, conn: ConnId, msg: Message) {
        if let Some(c) = self.conns.get(&conn) {
            let _ = c.tx.send(msg);
        }
    }

    fn send_json(&self, conn: ConnId, v: JsonValue) {
        self.send(conn, Message::Text(v.to_string().into()));
    }

    fn dispatch(&mut self, fx: Vec<Effect>) {
        for e in fx {
            match e {
                Effect::Send { conn, bytes } => self.send(conn, Message::Binary(bytes.into())),
                Effect::PeerReady { conn, peer } => {
                    if let Some(c) = self.conns.get_mut(&conn) {
                        c.peer = Some(hex::encode(peer));
                    }
                }
                Effect::Close { conn, reason } => {
                    warn!(conn, %reason, "closing connection");
                    self.send(
                        conn,
                        Message::Close(Some(CloseFrame {
                            code: 1008,
                            reason: reason.into(),
                        })),
                    );
                }
                Effect::Changed { paths } => debug!(n = paths.len(), "documents changed"),
                Effect::Synced { .. } => {}
            }
        }
    }

    fn signal(&mut self, conn: ConnId, text: &str) {
        let Ok(msg) = serde_json::from_str::<JsonValue>(text) else {
            return self.send_json(conn, json!({"t": "error", "error": "bad json"}));
        };
        let Some(me) = self.conns.get(&conn).and_then(|c| c.peer.clone()) else {
            return self.send_json(
                conn,
                json!({"t": "error", "error": "finish the sync handshake before signaling"}),
            );
        };
        match msg["t"].as_str() {
            Some("join") => {
                if let Some(old) = self.joined.insert(me.clone(), conn) {
                    if old != conn {
                        debug!(peer = %me, "peer rejoined on a new connection");
                    }
                }
                let peers: Vec<&String> = self.joined.keys().filter(|p| **p != me).collect();
                self.send_json(conn, json!({"t": "peers", "peers": peers}));
                for (p, c) in &self.joined {
                    if *p != me {
                        self.send_json(*c, json!({"t": "joined", "peer": me}));
                    }
                }
            }
            Some("signal") => {
                let Some(to) = msg["to"].as_str().and_then(|t| self.joined.get(t)) else {
                    return self.send_json(conn, json!({"t": "error", "error": "unknown peer"}));
                };
                self.send_json(*to, json!({"t": "signal", "from": me, "data": msg["data"]}));
            }
            _ => self.send_json(conn, json!({"t": "error", "error": "unknown message"})),
        }
    }

    fn leave(&mut self, conn: ConnId) {
        self.node.disconnect(conn);
        let Some(c) = self.conns.remove(&conn) else {
            return;
        };
        let Some(peer) = c.peer else { return };
        if self.joined.get(&peer) == Some(&conn) {
            self.joined.remove(&peer);
            for other in self.joined.values() {
                self.send_json(*other, json!({"t": "left", "peer": peer}));
            }
        }
    }
}

struct TokenBucket {
    tokens: f64,
    rate: f64,
    last: Instant,
}

impl TokenBucket {
    fn new(rate: u32) -> Self {
        TokenBucket {
            tokens: rate as f64 * 2.0,
            rate: rate as f64,
            last: Instant::now(),
        }
    }

    fn take(&mut self) -> bool {
        let now = Instant::now();
        self.tokens = (self.tokens + now.duration_since(self.last).as_secs_f64() * self.rate)
            .min(self.rate * 2.0);
        self.last = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

pub fn router(relay: Arc<Relay>) -> Router {
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/v1/{app}", get(ws_handler))
        .route("/v1/{app}/ice", get(ice_handler))
        .layer(tower_http::cors::CorsLayer::permissive())
        .with_state(relay)
}

async fn ice_handler(Path(app): Path<String>, State(relay): State<Arc<Relay>>) -> Response {
    if !relay.config.serves(&app) {
        return (StatusCode::NOT_FOUND, "unknown app").into_response();
    }
    Json(json!({ "iceServers": relay.config.ice_servers })).into_response()
}

async fn ws_handler(
    Path(app): Path<String>,
    ws: WebSocketUpgrade,
    State(relay): State<Arc<Relay>>,
) -> Response {
    if !relay.config.serves(&app) {
        return (StatusCode::NOT_FOUND, "unknown app").into_response();
    }
    let max = relay.config.max_frame_bytes;
    ws.max_message_size(max)
        .max_frame_size(max)
        .on_upgrade(move |s| handle_socket(relay, app, s))
}

async fn handle_socket(relay: Arc<Relay>, app: String, socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
    let writer = tokio::spawn(async move {
        while let Some(m) = rx.recv().await {
            let close = matches!(m, Message::Close(_));
            if sink.send(m).await.is_err() || close {
                break;
            }
        }
    });

    let conn = relay.next_conn.fetch_add(1, Ordering::Relaxed);
    let hub = relay.hub(&app);
    {
        let mut h = hub.lock().unwrap();
        h.conns.insert(
            conn,
            ConnHandle {
                tx: tx.clone(),
                peer: None,
            },
        );
        let fx = h.node.connect(
            conn,
            ConnOpts {
                sync: true,
                replicate: true,
            },
        );
        h.dispatch(fx);
    }
    debug!(app, conn, "connected");

    let mut bucket = TokenBucket::new(relay.config.max_frames_per_sec);
    while let Some(Ok(msg)) = stream.next().await {
        match msg {
            Message::Binary(b) => {
                if !bucket.take() {
                    let _ = tx.send(Message::Close(Some(CloseFrame {
                        code: 1008,
                        reason: "rate limit".into(),
                    })));
                    break;
                }
                let mut h = hub.lock().unwrap();
                let fx = h.node.receive(conn, &b, now_ms());
                h.dispatch(fx);
            }
            Message::Text(t) => hub.lock().unwrap().signal(conn, t.as_str()),
            Message::Close(_) => break,
            _ => {}
        }
    }

    hub.lock().unwrap().leave(conn);
    drop(tx);
    let _ = writer.await;
    debug!(app, conn, "disconnected");
}
