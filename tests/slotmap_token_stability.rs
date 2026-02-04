use indexmap::IndexSet;

mod slotmap;
use slotmap::_helpers::{
    SlotmapStressConfig, new_slotmap_engine, run_creation_drop_cycles,
    summarize_snapshot_for_snapshots,
};

#[test]
fn tokens_monotonic_and_not_reused_across_reuse() {
    let mut engine = new_slotmap_engine();
    let cfg = SlotmapStressConfig {
        nodes_per_cycle: 64,
        rounds: 3,
        base_value: 10,
    };
    let snapshots = run_creation_drop_cycles(&mut engine, cfg);

    let mut seen = IndexSet::new();
    let mut last = None;
    for snap in snapshots {
        for token in snap.tokens {
            let raw = token.raw_token();
            assert!(seen.insert(raw), "token {} 重复出现，说明发生了复用", raw);
            if let Some(prev) = last {
                assert!(
                    raw > prev,
                    "token 未严格递增: prev={} current={}",
                    prev,
                    raw
                );
            }
            last = Some(raw);
        }
    }

    let audit = engine.token_audit_snapshot();
    if let Some(last_tok) = last {
        assert_eq!(
            audit.last_deleted_token,
            Some(last_tok),
            "最近删除的 token 未记录"
        );
        assert!(
            audit.next_token > last_tok,
            "next_token 应大于已分配最大 token"
        );
    }
}

#[test]
fn token_debug_snapshot() {
    let mut engine = new_slotmap_engine();
    let cfg = SlotmapStressConfig {
        nodes_per_cycle: 6,
        rounds: 2,
        base_value: 1,
    };
    let snapshots = run_creation_drop_cycles(&mut engine, cfg);

    let mut lines = Vec::new();
    for (round, snap) in snapshots.iter().enumerate() {
        lines.push(format!("round #{round}"));
        lines.extend(summarize_snapshot_for_snapshots(snap));
    }

    insta::assert_snapshot!("token_debug__slotmap_basic", lines.join("\n"));
}
