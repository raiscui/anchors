mod slotmap;

use anchors::singlethread::Var;
use slotmap::_helpers::new_slotmap_engine;

/// 构建一组“混合类型”的节点：
/// - VarAnchor（由 Var::new/mount 产生）
/// - MapAnchor（由 watch.map(...) 产生）
fn build_var_watch_derived(
    seed: usize,
) -> (
    Var<usize>,
    anchors::singlethread::Anchor<usize>,
    anchors::singlethread::Anchor<usize>,
) {
    let var = Var::new(seed);
    let watch = var.watch();
    let derived = watch.map(|v| v + 1);
    (var, watch, derived)
}

/// 回归测试：反复 mount/remove 后，AnchorMemPool 必须产生 hit（说明确实发生复用）。
#[test]
fn anchor_pool_should_hit_after_reclaim() {
    let mut engine = new_slotmap_engine();
    let s0 = engine.anchor_pool_stats();

    // 第 1 轮：只会 miss（首次分配），随后 drop+stabilize 触发 recycle。
    let (var1, watch1, derived1) = build_var_watch_derived(10);
    assert_eq!(engine.get(&derived1), 11);
    drop(derived1);
    drop(watch1);
    drop(var1);
    engine.stabilize();

    let s1 = engine.anchor_pool_stats();
    assert!(
        s1.alloc_misses > s0.alloc_misses,
        "第一轮应发生系统分配（miss）"
    );
    assert!(
        s1.recycle_calls > s0.recycle_calls,
        "第一轮 drop+reclaim 后应发生 recycle"
    );

    // 第 2 轮：应出现至少一次 hit（复用上一轮 recycle 的 raw block）。
    let (var2, watch2, derived2) = build_var_watch_derived(20);
    assert_eq!(engine.get(&derived2), 21);
    drop(derived2);
    drop(watch2);
    drop(var2);
    engine.stabilize();

    let s2 = engine.anchor_pool_stats();
    assert!(
        s2.alloc_hits > s1.alloc_hits,
        "第二轮应出现 pool hit（alloc_hits 应增长），s1={:?} s2={:?}",
        s1,
        s2
    );
}

/// 回归测试：epoch（读窗口）内只允许 retire，禁止 reclaim/recycle。
#[test]
fn anchor_pool_should_not_recycle_within_epoch() {
    let mut engine = new_slotmap_engine();
    let s0 = engine.anchor_pool_stats();

    // 先创建并拉取一次，保证节点进入图与缓存输出建立。
    let (var1, watch1, derived1) = build_var_watch_derived(1);
    assert_eq!(engine.get(&derived1), 2);

    // 进入 epoch 后 drop：应 retire，不应 recycle。
    let epoch = engine.enter_epoch();
    drop(derived1);
    drop(watch1);
    drop(var1);

    engine.stabilize();
    let s1 = engine.anchor_pool_stats();
    assert_eq!(
        s1.recycle_calls, s0.recycle_calls,
        "epoch 内 stabilize 不应触发 recycle"
    );

    // epoch 内再创建一轮：由于没有 recycle，hit 不应增长（至少不应依赖 retire 的节点）。
    let (var2, watch2, derived2) = build_var_watch_derived(10);
    assert_eq!(engine.get(&derived2), 11);
    let s2 = engine.anchor_pool_stats();
    assert_eq!(
        s2.alloc_hits, s0.alloc_hits,
        "epoch 内不应出现来自 retire 的 hit（alloc_hits 不应增长）"
    );

    drop(derived2);
    drop(watch2);
    drop(var2);

    // epoch end：flush_retired 触发真正 reclaim，随后应能 recycle，并在下一轮出现 hit。
    drop(epoch);
    engine.stabilize();

    let s3 = engine.anchor_pool_stats();
    assert!(
        s3.recycle_calls > s0.recycle_calls,
        "epoch end 后应发生 reclaim/recycle"
    );

    let (var3, watch3, derived3) = build_var_watch_derived(100);
    assert_eq!(engine.get(&derived3), 101);
    let s4 = engine.anchor_pool_stats();
    assert!(
        s4.alloc_hits > s3.alloc_hits,
        "epoch end 后新建节点应能命中 pool（alloc_hits 应增长）"
    );

    drop(derived3);
    drop(watch3);
    drop(var3);
    engine.stabilize();
}
