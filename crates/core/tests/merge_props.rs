//! Property tests: LWW document merge converges regardless of delivery
//! order or duplication.

use aghanim_core::{Doc, Hlc, OpKind, Value};
use proptest::prelude::*;

#[derive(Debug, Clone)]
struct TestOp {
    kind: OpKind,
    hlc: Hlc,
    author: [u8; 32],
}

fn op_strategy() -> impl Strategy<Value = TestOp> {
    let field = prop_oneof![Just("a"), Just("b"), Just("c")].prop_map(String::from);
    let value = prop_oneof![
        any::<i64>().prop_map(Value::Int),
        "[a-z]{0,3}".prop_map(Value::Str),
        Just(Value::Null),
    ];
    let kind = prop_oneof![
        4 => (field.clone(), value).prop_map(|(field, value)| OpKind::Set { field, value }),
        1 => field.prop_map(|field| OpKind::DelField { field }),
        1 => Just(OpKind::DelDoc),
    ];
    // Small ranges force plenty of timestamp and author ties.
    (kind, 0u64..6, 0u16..2, 0u8..3).prop_map(|(kind, ms, c, a)| TestOp {
        kind,
        hlc: Hlc::new(ms, c),
        author: [a; 32],
    })
}

fn apply_all(ops: &[TestOp]) -> Doc {
    let mut d = Doc::default();
    for o in ops {
        d.apply(&o.kind, o.hlc, o.author);
    }
    d
}

proptest! {
    #[test]
    fn order_does_not_matter(ops in prop::collection::vec(op_strategy(), 0..24), seed in any::<u64>()) {
        let mut shuffled = ops.clone();
        // Deterministic Fisher-Yates driven by the seed.
        let mut s = seed | 1;
        for i in (1..shuffled.len()).rev() {
            s ^= s << 13; s ^= s >> 7; s ^= s << 17;
            shuffled.swap(i, (s % (i as u64 + 1)) as usize);
        }
        let a = apply_all(&ops);
        let b = apply_all(&shuffled);
        prop_assert_eq!(a.visible_fields(), b.visible_fields());
        prop_assert_eq!(a, b);
    }

    #[test]
    fn duplicates_do_not_matter(ops in prop::collection::vec(op_strategy(), 0..24)) {
        let mut doubled = ops.clone();
        doubled.extend(ops.iter().rev().cloned());
        prop_assert_eq!(apply_all(&ops), apply_all(&doubled));
    }

    #[test]
    fn merging_two_replicas_matches_one_replica(
        left in prop::collection::vec(op_strategy(), 0..16),
        right in prop::collection::vec(op_strategy(), 0..16),
    ) {
        // Replica A sees left then right; replica B sees right then left.
        let mut ab = left.clone(); ab.extend(right.clone());
        let mut ba = right; ba.extend(left);
        prop_assert_eq!(apply_all(&ab), apply_all(&ba));
    }
}
