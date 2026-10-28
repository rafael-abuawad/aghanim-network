mod common;

use aghanim_core::{Change, Hlc, Op, OpKind, Value};
use aghanim_crypto::{sign_change, Identity};
use aghanim_sync::msg::Msg;
use aghanim_sync::{ConnOpts, Error, Write};
use common::*;

const DOC: &str = "public/app/todos/t1";

#[test]
fn live_sync_between_two_peers() {
    let mut net = Net::new(2, 1);
    net.subscribe(0, "public/app");
    net.subscribe(1, "public/app");
    net.connect(0, 1);
    net.run();
    net.write(0, vec![set(DOC, &[("title", s("milk"))])])
        .unwrap();
    net.run();
    assert_eq!(net.nodes[1].get(DOC).unwrap().unwrap()["title"], s("milk"));
    assert!(net.changed[1].contains(&DOC.to_string()));
}

#[test]
fn catch_up_after_offline_edits_and_cursor_resume() {
    let mut net = Net::new(2, 2);
    net.subscribe(0, "public/app");
    net.subscribe(1, "public/app");
    for i in 0..50 {
        net.write(
            0,
            vec![set(
                &format!("public/app/todos/t{i}"),
                &[("n", Value::Int(i))],
            )],
        )
        .unwrap();
    }
    let link = net.connect(0, 1);
    net.run();
    let q = aghanim_core::Query::new("public/app/todos");
    assert_eq!(net.nodes[1].query(&q).unwrap().len(), 50);

    // Disconnect, write a little more on both sides, reconnect: only the gap
    // is transferred.
    net.cut(link);
    net.write(
        0,
        vec![set("public/app/todos/t0", &[("n", Value::Int(100))])],
    )
    .unwrap();
    net.write(
        1,
        vec![set("public/app/todos/t1", &[("n", Value::Int(200))])],
    )
    .unwrap();
    let before = net.frames_delivered;
    net.connect(0, 1);
    net.run();
    // hello+proof each way, sub+changes+synced each way, plus a few extra.
    assert!(
        net.frames_delivered - before < 12,
        "resync sent {} frames",
        net.frames_delivered - before
    );
    for n in 0..2 {
        assert_eq!(
            net.nodes[n].get("public/app/todos/t0").unwrap().unwrap()["n"],
            Value::Int(100)
        );
        assert_eq!(
            net.nodes[n].get("public/app/todos/t1").unwrap().unwrap()["n"],
            Value::Int(200)
        );
    }
}

#[test]
fn set_update_delete_semantics() {
    let mut net = Net::new(1, 3);
    net.write(
        0,
        vec![set(DOC, &[("a", Value::Int(1)), ("b", Value::Int(2))])],
    )
    .unwrap();
    // Set without merge replaces the document.
    net.write(
        0,
        vec![Write::Set {
            path: DOC.into(),
            fields: [("c".into(), Some(Value::Int(3)))].into(),
            merge: false,
        }],
    )
    .unwrap();
    let doc = net.nodes[0].get(DOC).unwrap().unwrap();
    assert_eq!(doc.keys().collect::<Vec<_>>(), ["c"]);
    // Update deletes a field with None.
    net.write(
        0,
        vec![Write::Update {
            path: DOC.into(),
            fields: [("c".into(), None), ("d".into(), Some(Value::Null))].into(),
        }],
    )
    .unwrap();
    assert_eq!(
        net.nodes[0]
            .get(DOC)
            .unwrap()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        ["d"]
    );
    net.write(0, vec![Write::Delete { path: DOC.into() }])
        .unwrap();
    assert_eq!(net.nodes[0].get(DOC).unwrap(), None);
    let err = net.write(
        0,
        vec![Write::Update {
            path: DOC.into(),
            fields: Default::default(),
        }],
    );
    assert!(matches!(err, Err(Error::NotFound(_))));
}

#[test]
fn user_space_is_owner_only() {
    let mut net = Net::new(2, 4);
    let alice = net.user_hex(0);
    let path = format!("users/{alice}/profile/main");
    net.write(0, vec![set(&path, &[("name", s("Alice"))])])
        .unwrap();
    let err = net.write(1, vec![set(&path, &[("name", s("Mallory"))])]);
    assert!(matches!(err, Err(Error::PermissionDenied(_))), "{err:?}");
}

/// Sends a hand-crafted change from node `from` over `link`, bypassing its
/// local checks, as a malicious peer would.
fn inject(net: &mut Net, from: usize, link: u32, scope: &str, change: Change) {
    let bytes = Msg::Changes {
        scope: scope.into(),
        items: vec![(999, change)],
    }
    .to_bytes();
    let to = if net.links[&link].a == from {
        net.links[&link].b
    } else {
        net.links[&link].a
    };
    let fx = net.nodes[to].receive(link, &bytes, net.now);
    net.handle(to, fx);
}

#[test]
fn forged_and_unauthorized_remote_changes_are_rejected() {
    let mut net = Net::new(2, 5);
    let alice = net.user_hex(0);
    let scope = format!("users/{alice}");
    net.subscribe(0, &scope);
    net.subscribe(1, &scope);
    let link = net.connect(0, 1);
    net.run();

    let mallory = Identity::random();
    let op = |v: &str| Op {
        path: format!("{scope}/profile/main"),
        hlc: Hlc::new(net.now, 0),
        kind: OpKind::Set {
            field: "name".into(),
            value: s(v),
        },
    };
    // Validly signed by Mallory, but Mallory doesn't own alice's space.
    let c1 = sign_change(&mallory, vec![op("mallory")]);
    // Claims to be Alice, with Mallory's signature.
    let mut c2 = sign_change(&mallory, vec![op("forged")]);
    c2.author = net.nodes[0].user().unwrap().public();
    // Timestamped an hour in the future to win every conflict.
    let mut future = op("future");
    future.hlc = Hlc::new(net.now + 3_600_000, 0);
    let me = net.nodes[1].user().unwrap().clone();
    let c3 = sign_change(
        &me,
        vec![Op {
            path: "public/x/c/d".into(),
            ..future
        }],
    );

    for c in [c1.clone(), c2.clone()] {
        inject(&mut net, 1, link, &scope, c);
    }
    inject(&mut net, 1, link, "public/x", c3.clone());
    for c in [&c1, &c2, &c3] {
        assert!(!net.nodes[0].has_change(&c.hash()));
    }
    assert_eq!(
        net.nodes[0].get(&format!("{scope}/profile/main")).unwrap(),
        None
    );
}

#[test]
fn handshake_rejects_wrong_app() {
    let mut net = Net::new(1, 6);
    let mut other = aghanim_sync::Node::open(
        aghanim_store::MemoryKv::new(),
        aghanim_sync::Config::new("other-app"),
    );
    let fx = other.connect(1, ConnOpts::default());
    let hello = match &fx[0] {
        aghanim_sync::Effect::Send { bytes, .. } => bytes.clone(),
        e => panic!("{e:?}"),
    };
    net.nodes[0].connect(1, ConnOpts::default());
    let fx = net.nodes[0].receive(1, &hello, 0);
    assert!(fx
        .iter()
        .any(|e| matches!(e, aghanim_sync::Effect::Close { .. })));
}

fn space_setup(net: &mut Net) -> String {
    let (id, fx) = net.nodes[0].create_space("team", net.now).unwrap();
    net.handle(0, fx);
    let scope = format!("spaces/{id}");
    for i in 1..net.nodes.len() {
        net.subscribe(i, &scope);
    }
    id
}

#[test]
fn encrypted_space_lifecycle() {
    // 0 = owner, 1 = member, 2 = outsider, 3 = relay
    let mut net = Net::new(3, 7);
    let relay = net.add_relay();
    let id = space_setup(&mut net);
    for i in 0..3 {
        net.connect(i, relay);
    }
    net.run();
    let bob = net.user_hex(1);
    let fx = net.nodes[0].invite(&id, &bob, "writer", net.now).unwrap();
    net.handle(0, fx);
    net.run();

    let doc = format!("spaces/{id}/todos/t1");
    net.write(0, vec![set(&doc, &[("title", s("old secret"))])])
        .unwrap();
    net.run();
    assert_eq!(
        net.nodes[1].get(&doc).unwrap().unwrap()["title"],
        s("old secret")
    );
    // The outsider and the relay hold the document, but only as ciphertext.
    assert_eq!(net.nodes[2].get(&doc).unwrap(), None);
    let raw = net.nodes[relay].store.get_doc(&doc).unwrap();
    assert!(matches!(raw.get("title"), Some(Value::Encrypted(_))));

    // Bob writes too; the outsider can't.
    net.write(1, vec![set(&doc, &[("by", s("bob"))])]).unwrap();
    net.run();
    assert_eq!(net.nodes[0].get(&doc).unwrap().unwrap()["by"], s("bob"));
    assert!(net.write(2, vec![set(&doc, &[("x", s("y"))])]).is_err());

    // Remove Bob: the key rotates, so he can't read anything new.
    let fx = net.nodes[0].remove_member(&id, &bob, net.now).unwrap();
    net.handle(0, fx);
    net.run();
    net.write(0, vec![set(&doc, &[("plan", s("new secret"))])])
        .unwrap();
    net.run();
    let bob_view = net.nodes[1].get(&doc).unwrap().unwrap();
    assert_eq!(bob_view.get("title"), Some(&s("old secret")));
    assert_eq!(bob_view.get("plan"), None);
    assert_eq!(
        net.nodes[0].get(&doc).unwrap().unwrap()["plan"],
        s("new secret")
    );
    assert!(net.write(1, vec![set(&doc, &[("z", s("late"))])]).is_err());
    let members = net.nodes[0].members(&id).unwrap();
    assert!(members.iter().any(|m| m.pubkey == bob && !m.active));
}

#[test]
fn member_write_before_grant_is_held_then_applied() {
    let mut net = Net::new(3, 8);
    let id = space_setup(&mut net);
    // 0 and 1 sync; 2 is offline.
    let l01 = net.connect(0, 1);
    net.run();
    let bob = net.user_hex(1);
    let fx = net.nodes[0].invite(&id, &bob, "writer", net.now).unwrap();
    net.handle(0, fx);
    net.run();
    let doc = format!("spaces/{id}/notes/n");
    net.write(1, vec![set(&doc, &[("t", s("hi"))])]).unwrap();
    net.run();
    net.cut(l01);

    // Node 2 hears from Bob directly and gets Bob's write before the grant.
    let scope = format!("spaces/{id}");
    let log = net.nodes[1].store.changes_after(&scope, 0, 100);
    assert_eq!(log.len(), 3, "create, invite, write");
    let (create, invite, write) = (log[0].1.clone(), log[1].1.clone(), log[2].1.clone());
    let quiet = ConnOpts {
        sync: false,
        replicate: false,
    };
    let l12 = net.connect_with(1, 2, quiet, quiet);
    net.run();
    inject(&mut net, 1, l12, &scope, create);
    inject(&mut net, 1, l12, &scope, write.clone());
    assert!(!net.nodes[2].has_change(&write.hash()));
    assert_eq!(net.nodes[2].pending_count(), 1);
    inject(&mut net, 1, l12, &scope, invite);
    assert!(net.nodes[2].has_change(&write.hash()));
    assert_eq!(net.nodes[2].pending_count(), 0);
    assert!(net.nodes[2].store.get_doc(&doc).is_some());
}
