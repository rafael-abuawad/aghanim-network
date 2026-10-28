//! In-memory network of sync nodes with controllable delivery.
#![allow(dead_code)]

use std::collections::{BTreeMap, VecDeque};

use aghanim_core::Value;
use aghanim_crypto::{Identity, KdfParams};
use aghanim_store::MemoryKv;
use aghanim_sync::{Config, ConnId, ConnOpts, Effect, Node, Write};

pub const APP: &str = "test-app";

pub struct Link {
    pub a: usize,
    pub b: usize,
    /// Pending frames: (deliver_at, bytes). Index 0 is a→b, 1 is b→a.
    queues: [VecDeque<(u64, Vec<u8>)>; 2],
}

pub struct Net {
    pub nodes: Vec<Node<MemoryKv>>,
    pub links: BTreeMap<ConnId, Link>,
    next_link: ConnId,
    pub now: u64,
    rng: u64,
    pub max_delay: u64,
    /// Every `Changed` path each node reported.
    pub changed: Vec<Vec<String>>,
    pub frames_delivered: usize,
}

impl Net {
    pub fn new(n: usize, seed: u64) -> Self {
        let mut nodes = Vec::new();
        for _ in 0..n {
            let mut node = Node::open(MemoryKv::new(), Config::new(APP));
            node.set_user(Some(Identity::random()), false);
            nodes.push(node);
        }
        Net {
            nodes,
            links: BTreeMap::new(),
            next_link: 1,
            now: 1_700_000_000_000,
            rng: seed.wrapping_mul(0x9E3779B97F4A7C15) | 1,
            max_delay: 0,
            changed: vec![vec![]; n],
            frames_delivered: 0,
        }
    }

    pub fn add_relay(&mut self) -> usize {
        let mut cfg = Config::new(APP);
        cfg.replicate_all = true;
        self.nodes.push(Node::open(MemoryKv::new(), cfg));
        self.changed.push(vec![]);
        self.nodes.len() - 1
    }

    pub fn rand(&mut self) -> u64 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        self.rng
    }

    pub fn user_hex(&self, i: usize) -> String {
        self.nodes[i].user().unwrap().public_hex()
    }

    pub fn connect(&mut self, a: usize, b: usize) -> ConnId {
        self.connect_with(a, b, ConnOpts::default(), ConnOpts::default())
    }

    pub fn connect_with(&mut self, a: usize, b: usize, oa: ConnOpts, ob: ConnOpts) -> ConnId {
        let id = self.next_link;
        self.next_link += 1;
        self.links.insert(
            id,
            Link {
                a,
                b,
                queues: [VecDeque::new(), VecDeque::new()],
            },
        );
        let fa = self.nodes[a].connect(id, oa);
        self.handle(a, fa);
        let fb = self.nodes[b].connect(id, ob);
        self.handle(b, fb);
        id
    }

    pub fn cut(&mut self, link: ConnId) {
        if let Some(l) = self.links.remove(&link) {
            self.nodes[l.a].disconnect(link);
            self.nodes[l.b].disconnect(link);
        }
    }

    pub fn cut_all(&mut self) {
        for id in self.links.keys().copied().collect::<Vec<_>>() {
            self.cut(id);
        }
    }

    pub fn connect_all(&mut self) {
        for a in 0..self.nodes.len() {
            for b in a + 1..self.nodes.len() {
                self.connect(a, b);
            }
        }
    }

    pub fn handle(&mut self, from: usize, fx: Vec<Effect>) {
        for e in fx {
            match e {
                Effect::Send { conn, bytes } => {
                    let delay = if self.max_delay == 0 {
                        0
                    } else {
                        self.rand() % self.max_delay
                    };
                    let now = self.now;
                    let Some(l) = self.links.get_mut(&conn) else {
                        continue;
                    };
                    let dir = if l.a == from { 0 } else { 1 };
                    // Per-connection FIFO, like a reliable ordered channel.
                    let at = l.queues[dir].back().map_or(0, |x| x.0).max(now + delay);
                    l.queues[dir].push_back((at, bytes));
                }
                Effect::Close { conn, reason } => {
                    panic!("node {from} closed link {conn}: {reason}");
                }
                Effect::Changed { paths } => self.changed[from].extend(paths),
                Effect::PeerReady { .. } | Effect::Synced { .. } => {}
            }
        }
    }

    /// Delivers the earliest pending frame. Returns false when idle.
    pub fn step(&mut self) -> bool {
        let mut best: Option<(u64, u64, ConnId, usize)> = None;
        let ids: Vec<ConnId> = self.links.keys().copied().collect();
        for id in ids {
            for dir in 0..2 {
                if let Some(&(at, _)) = self.links[&id].queues[dir].front() {
                    let tie = self.rand();
                    if best.is_none_or(|b| (at, tie) < (b.0, b.1)) {
                        best = Some((at, tie, id, dir));
                    }
                }
            }
        }
        let Some((at, _, id, dir)) = best else {
            return false;
        };
        let l = self.links.get_mut(&id).unwrap();
        let (_, bytes) = l.queues[dir].pop_front().unwrap();
        let to = if dir == 0 { l.b } else { l.a };
        self.now = self.now.max(at);
        let fx = self.nodes[to].receive(id, &bytes, self.now);
        self.frames_delivered += 1;
        self.handle(to, fx);
        true
    }

    pub fn run(&mut self) {
        let mut guard = 0;
        while self.step() {
            guard += 1;
            assert!(guard < 1_000_000, "network did not quiesce");
        }
    }

    pub fn write(&mut self, i: usize, writes: Vec<Write>) -> Result<(), aghanim_sync::Error> {
        self.now += 1;
        let fx = self.nodes[i].write(writes, self.now)?;
        self.handle(i, fx);
        Ok(())
    }

    pub fn subscribe(&mut self, i: usize, scope: &str) {
        let fx = self.nodes[i].subscribe(scope).unwrap();
        self.handle(i, fx);
    }
}

pub fn set(path: &str, pairs: &[(&str, Value)]) -> Write {
    Write::Set {
        path: path.into(),
        fields: pairs
            .iter()
            .map(|(k, v)| (k.to_string(), Some(v.clone())))
            .collect(),
        merge: true,
    }
}

pub fn s(v: &str) -> Value {
    Value::Str(v.into())
}

pub fn login(alias: &str) -> Identity {
    aghanim_crypto::identity_from_passphrase(alias, "pw", KdfParams::INSECURE_FAST).unwrap()
}
