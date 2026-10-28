//! Deterministic network simulation.
//!
//! Peers write concurrently while links are randomly delayed, dropped, and
//! partitioned, and a malicious peer injects forged changes. After the
//! network heals, every peer must hold identical documents and none may hold
//! a forged change.

mod common;

use aghanim_core::{Hash, Hlc, Op, OpKind, Value};
use aghanim_crypto::{sign_change, Identity};
use aghanim_sync::msg::Msg;
use aghanim_sync::Write;
use common::*;

const PEERS: usize = 5;
const DOCS: u64 = 6;

fn simulate(seed: u64) {
    let mut net = Net::new(PEERS, seed);
    net.max_delay = 40;
    let relay = net.add_relay();

    // Setup: node 0 owns a space; 1..3 are writers, 4 is a reader.
    let (space, fx) = net.nodes[0].create_space("sim", net.now).unwrap();
    net.handle(0, fx);
    for i in 0..PEERS {
        net.subscribe(i, "public/sim");
        net.subscribe(i, &format!("spaces/{space}"));
    }
    net.connect_all();
    net.run();
    for i in 1..PEERS {
        let role = if i == PEERS - 1 { "reader" } else { "writer" };
        let pk = net.user_hex(i);
        let fx = net.nodes[0].invite(&space, &pk, role, net.now).unwrap();
        net.handle(0, fx);
    }
    net.run();

    let mallory = Identity::random();
    let mut forged: Vec<Hash> = Vec::new();
    let mut writes_ok = 0;

    for _ in 0..300 {
        match net.rand() % 12 {
            0..=5 => {
                let who = (net.rand() % PEERS as u64) as usize;
                let doc = net.rand() % DOCS;
                let path = if net.rand().is_multiple_of(2) {
                    format!("public/sim/items/d{doc}")
                } else {
                    format!("spaces/{space}/items/d{doc}")
                };
                let field = ["a", "b", "c"][(net.rand() % 3) as usize].to_string();
                let w = match net.rand() % 8 {
                    0 => Write::Delete { path },
                    1 => Write::Set {
                        path,
                        fields: [(field, None)].into(),
                        merge: true,
                    },
                    2 => Write::Set {
                        path,
                        fields: [(field, Some(Value::Int(net.rand() as i64 % 100)))].into(),
                        merge: false,
                    },
                    _ => set(&path, &[(&field, Value::Int(net.rand() as i64 % 100))]),
                };
                // Reader writes to the space fail locally; that's expected.
                if net.write(who, vec![w]).is_ok() {
                    writes_ok += 1;
                }
            }
            6 | 7 => {
                let ids: Vec<u32> = net.links.keys().copied().collect();
                if !ids.is_empty() {
                    let id = ids[(net.rand() % ids.len() as u64) as usize];
                    net.cut(id);
                }
            }
            8 => {
                let a = (net.rand() % net.nodes.len() as u64) as usize;
                let b = (net.rand() % net.nodes.len() as u64) as usize;
                if a != b {
                    net.connect(a, b);
                }
            }
            9 => {
                // Partition: cut every link crossing a random split.
                let mask = net.rand();
                let crossing: Vec<u32> = net
                    .links
                    .iter()
                    .filter(|(_, l)| (mask >> l.a) & 1 != (mask >> l.b) & 1)
                    .map(|(id, _)| *id)
                    .collect();
                for id in crossing {
                    net.cut(id);
                }
            }
            10 => {
                // Mallory injects changes: one claiming to be node 1, one
                // writing into the space without membership.
                let ids: Vec<u32> = net.links.keys().copied().collect();
                if ids.is_empty() {
                    continue;
                }
                let id = ids[(net.rand() % ids.len() as u64) as usize];
                let to = net.links[&id].b;
                if net.nodes[to].peer_of(id).is_none() {
                    continue; // handshake still in flight
                }
                let op = |path: String| Op {
                    path,
                    hlc: Hlc::new(net.now, 0),
                    kind: OpKind::Set {
                        field: "a".into(),
                        value: Value::Str("pwned".into()),
                    },
                };
                let mut fake = sign_change(&mallory, vec![op("public/sim/items/d0".into())]);
                fake.author = net.nodes[1].user().unwrap().public();
                let intruder = sign_change(&mallory, vec![op(format!("spaces/{space}/items/d1"))]);
                for (scope, c) in [
                    ("public/sim".to_string(), fake),
                    (format!("spaces/{space}"), intruder),
                ] {
                    forged.push(c.hash());
                    let bytes = Msg::Changes {
                        scope,
                        items: vec![(1, c)],
                    }
                    .to_bytes();
                    let fx = net.nodes[to].receive(id, &bytes, net.now);
                    net.handle(to, fx);
                }
            }
            _ => {}
        }
        for _ in 0..net.rand() % 6 {
            net.step();
        }
    }

    // Heal and let everything settle.
    net.cut_all();
    net.connect_all();
    net.run();

    assert!(
        writes_ok > 50,
        "seed {seed}: only {writes_ok} writes succeeded"
    );
    for d in 0..DOCS {
        for path in [
            format!("public/sim/items/d{d}"),
            format!("spaces/{space}/items/d{d}"),
        ] {
            let reference = net.nodes[0].store.get_doc(&path);
            let view = net.nodes[0].get(&path).unwrap();
            for i in 1..net.nodes.len() {
                assert_eq!(
                    net.nodes[i].store.get_doc(&path),
                    reference,
                    "seed {seed}: node {i} diverged on {path}"
                );
                if i != relay {
                    assert_eq!(
                        net.nodes[i].get(&path).unwrap(),
                        view,
                        "seed {seed}: node {i} sees {path} differently"
                    );
                }
            }
        }
    }
    for (i, n) in net.nodes.iter().enumerate() {
        for h in &forged {
            assert!(
                !n.has_change(h),
                "seed {seed}: node {i} accepted a forged change"
            );
        }
        // Intruder writes to the space look like "grant not seen yet", so
        // they sit in the bounded pending queue; nothing else should.
        assert!(
            n.pending_count() <= forged.len() / 2,
            "seed {seed}: node {i} holds legit pending changes"
        );
    }
}

#[test]
fn converges_under_chaos() {
    for seed in 1..=40 {
        simulate(seed);
    }
}
