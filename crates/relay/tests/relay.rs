//! Integration tests: native clients talking to a real relay over WebSockets.

use std::sync::Arc;
use std::time::Duration;

use aghanim_core::{Hlc, Op, OpKind, Value};
use aghanim_crypto::{sign_change, Identity};
use aghanim_relay::{Relay, RelayConfig};
use aghanim_store::MemoryKv;
use aghanim_sync::msg::Msg;
use aghanim_sync::{Config, ConnOpts, Effect, Node, Write};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

const APP: &str = "relay-test";

struct Server {
    addr: std::net::SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

impl Server {
    async fn start(dir: &std::path::Path) -> Server {
        let cfg = RelayConfig {
            data_dir: dir.to_owned(),
            apps: vec![APP.into()],
            ..Default::default()
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // Upgraded WebSocket tasks from a previous in-process server can hold
        // the database for a moment after shutdown; a real restart is a new
        // process, so just retry.
        let mut tries = 0;
        let relay: Arc<Relay> = loop {
            match Relay::open(cfg.clone()) {
                Ok(r) => break r,
                Err(e) if tries < 50 => {
                    tries += 1;
                    let _ = e;
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(e) => panic!("{e}"),
            }
        };
        let (stop, rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            aghanim_relay::serve(listener, relay, async {
                let _ = rx.await;
            })
            .await
            .unwrap();
        });
        Server {
            addr,
            stop: Some(stop),
            task,
        }
    }

    async fn stop(mut self) {
        let _ = self.stop.take().unwrap().send(());
        let _ = tokio::time::timeout(Duration::from_secs(5), self.task).await;
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

struct Client {
    node: Node<MemoryKv>,
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    texts: Vec<serde_json::Value>,
    ready: bool,
}

impl Client {
    async fn connect(addr: std::net::SocketAddr, scopes: &[&str]) -> Client {
        let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/v1/{APP}"))
            .await
            .unwrap();
        let mut node = Node::open(MemoryKv::new(), Config::new(APP));
        node.set_user(Some(Identity::random()), false);
        let mut c = Client {
            node,
            ws,
            texts: vec![],
            ready: false,
        };
        for s in scopes {
            let fx = c.node.subscribe(s).unwrap();
            c.apply(fx).await;
        }
        let fx = c.node.connect(1, ConnOpts::default());
        c.apply(fx).await;
        c.pump_until(|c| c.ready).await;
        c
    }

    async fn apply(&mut self, fx: Vec<Effect>) {
        for e in fx {
            match e {
                Effect::Send { bytes, .. } => {
                    self.ws.send(Message::Binary(bytes.into())).await.unwrap()
                }
                Effect::PeerReady { .. } => self.ready = true,
                Effect::Close { reason, .. } => panic!("client closed: {reason}"),
                _ => {}
            }
        }
    }

    /// Processes incoming frames until `done` holds (or panics after 5 s).
    async fn pump_until(&mut self, done: impl Fn(&Client) -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !done(self) {
            let msg = tokio::time::timeout_at(deadline, self.ws.next())
                .await
                .expect("timed out waiting for condition")
                .expect("socket closed")
                .unwrap();
            match msg {
                Message::Binary(b) => {
                    let fx = self.node.receive(1, &b, now_ms());
                    self.apply(fx).await;
                }
                Message::Text(t) => self.texts.push(serde_json::from_str(t.as_str()).unwrap()),
                Message::Close(f) => panic!("server closed: {f:?}"),
                _ => {}
            }
        }
    }

    /// Processes frames for a short while, for asserting that nothing arrives.
    async fn pump_for(&mut self, d: Duration) {
        let _ = tokio::time::timeout(d, self.pump_until(|_| false)).await;
    }

    async fn write(&mut self, path: &str, field: &str, v: Value) {
        let w = Write::Set {
            path: path.into(),
            fields: [(field.into(), Some(v))].into(),
            merge: true,
        };
        // Keep reading briefly first: the relay's reciprocal `Sub` may still
        // be in flight, and a live client would have processed it.
        self.pump_for(Duration::from_millis(100)).await;
        let fx = self.node.write(vec![w], now_ms()).unwrap();
        self.apply(fx).await;
    }

    fn get(&self, path: &str, field: &str) -> Option<Value> {
        self.node
            .get(path)
            .unwrap()
            .and_then(|f| f.get(field).cloned())
    }

    async fn send_text(&mut self, v: serde_json::Value) {
        self.ws
            .send(Message::Text(v.to_string().into()))
            .await
            .unwrap();
    }
}

const DOC: &str = "public/room/msgs/m1";

#[tokio::test]
async fn two_clients_sync_through_relay_and_data_persists() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(dir.path()).await;
    let mut a = Client::connect(server.addr, &["public/room"]).await;
    let mut b = Client::connect(server.addr, &["public/room"]).await;

    a.write(DOC, "text", Value::Str("hello".into())).await;
    b.pump_until(|b| b.get(DOC, "text").is_some()).await;
    assert_eq!(b.get(DOC, "text"), Some(Value::Str("hello".into())));

    b.write(DOC, "reply", Value::Str("hi".into())).await;
    a.pump_until(|a| a.get(DOC, "reply").is_some()).await;

    // Restart the relay on the same data dir: a brand-new client still gets
    // everything.
    drop((a, b));
    server.stop().await;
    let server = Server::start(dir.path()).await;
    let mut c = Client::connect(server.addr, &["public/room"]).await;
    c.pump_until(|c| c.get(DOC, "reply").is_some()).await;
    assert_eq!(c.get(DOC, "text"), Some(Value::Str("hello".into())));
    server.stop().await;
}

#[tokio::test]
async fn relay_rejects_forged_changes() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(dir.path()).await;
    let mut victim = Client::connect(server.addr, &[]).await;
    let scope = format!("users/{}", victim.node.user().unwrap().public_hex());
    let fx = victim.node.subscribe(&scope).unwrap();
    victim.apply(fx).await;
    let mut mallory = Client::connect(server.addr, &[&scope]).await;

    // Mallory signs a write into the victim's user space and sends it raw.
    let c = sign_change(
        &Identity::random(),
        vec![Op {
            path: format!("{scope}/profile/main"),
            hlc: Hlc::new(now_ms(), 0),
            kind: OpKind::Set {
                field: "name".into(),
                value: Value::Str("pwned".into()),
            },
        }],
    );
    let frame = Msg::Changes {
        scope: scope.clone(),
        items: vec![(1, c.clone())],
    }
    .to_bytes();
    mallory
        .ws
        .send(Message::Binary(frame.into()))
        .await
        .unwrap();

    victim.pump_for(Duration::from_millis(500)).await;
    assert!(!victim.node.has_change(&c.hash()));
    // A fresh client syncing the scope from the relay doesn't get it either.
    let mut fresh = Client::connect(server.addr, &[&scope]).await;
    fresh.pump_for(Duration::from_millis(300)).await;
    assert!(!fresh.node.has_change(&c.hash()));
    server.stop().await;
}

#[tokio::test]
async fn signaling_requires_handshake_and_forwards_between_peers() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(dir.path()).await;
    let mut a = Client::connect(server.addr, &[]).await;
    let mut b = Client::connect(server.addr, &[]).await;
    let a_id = hex::encode(a.node.device_id());
    let b_id = hex::encode(b.node.device_id());

    a.send_text(serde_json::json!({"t": "join"})).await;
    a.pump_until(|a| a.texts.iter().any(|t| t["t"] == "peers"))
        .await;
    b.send_text(serde_json::json!({"t": "join"})).await;
    b.pump_until(|b| b.texts.iter().any(|t| t["t"] == "peers"))
        .await;
    assert_eq!(b.texts.last().unwrap()["peers"], serde_json::json!([a_id]));
    a.pump_until(|a| {
        a.texts
            .iter()
            .any(|t| t["t"] == "joined" && t["peer"] == b_id)
    })
    .await;

    b.send_text(serde_json::json!({"t": "signal", "to": a_id, "data": {"sdp": "x"}}))
        .await;
    a.pump_until(|a| a.texts.iter().any(|t| t["t"] == "signal"))
        .await;
    let sig = a.texts.iter().find(|t| t["t"] == "signal").unwrap();
    assert_eq!(sig["from"], b_id);
    assert_eq!(sig["data"]["sdp"], "x");

    drop(b);
    a.pump_until(|a| {
        a.texts
            .iter()
            .any(|t| t["t"] == "left" && t["peer"] == b_id)
    })
    .await;

    // A raw socket that skipped the handshake can't signal.
    let (mut raw, _) = tokio_tungstenite::connect_async(format!("ws://{}/v1/{APP}", server.addr))
        .await
        .unwrap();
    raw.send(Message::Text(r#"{"t":"join"}"#.into()))
        .await
        .unwrap();
    let reply = loop {
        match raw.next().await.unwrap().unwrap() {
            Message::Text(t) => {
                break serde_json::from_str::<serde_json::Value>(t.as_str()).unwrap()
            }
            _ => continue,
        }
    };
    assert_eq!(reply["t"], "error");
    server.stop().await;
}

#[tokio::test]
async fn ice_config_and_unknown_apps() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(dir.path()).await;
    let body = http_get(server.addr, &format!("/v1/{APP}/ice")).await;
    assert!(body.contains("stun:stun.l.google.com:19302"), "{body}");
    let missing = http_get(server.addr, "/v1/other-app/ice").await;
    assert!(missing.starts_with("HTTP/1.1 404"), "{missing}");
    server.stop().await;
}

async fn http_get(addr: std::net::SocketAddr, path: &str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).await.unwrap();
    out
}
