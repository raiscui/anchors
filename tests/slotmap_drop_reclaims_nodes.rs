//! US1: 节点能被回收，alloc/free 成对出现并维持 free list 非空。
//! 运行方式：
//!   cargo nextest run -p anchors --features anchors_slotmap --test slotmap_drop_reclaims_nodes --profile anchors-slotmap

mod slotmap;

use slotmap::_helpers::{SlotmapStressConfig, new_slotmap_engine, run_creation_drop_cycles};

#[test]
fn slotmap_drop_reclaims_nodes() {
    let mut engine = new_slotmap_engine();
    // 规模：1_000 * 10，足够触发 free list 循环复用。
    let cfg = SlotmapStressConfig {
        nodes_per_cycle: 1_000,
        rounds: 10,
        base_value: 1,
    };

    let snapshots = run_creation_drop_cycles(&mut engine, cfg);

    // 验证每轮计算出的值与 seeds 一致，确保 Engine::get 正常。
    for (i, snap) in snapshots.iter().enumerate() {
        let expected_first = cfg.base_value + i;
        assert_eq!(
            snap.values.first().copied().unwrap_or_default(),
            expected_first
        );
    }

    // 重点：token 应单调递增且不重复（简单冒泡检查相邻轮次）。
    for win in snapshots.windows(2) {
        let prev = &win[0].tokens;
        let next = &win[1].tokens;
        assert!(
            next.iter().all(|tok| !prev.contains(tok)),
            "token reuse detected"
        );
    }
}
