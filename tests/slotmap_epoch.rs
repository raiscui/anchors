mod slotmap;

use anchors::singlethread::Var;

use slotmap::_helpers::new_slotmap_engine;

/// 在 epoch（读窗口）内，token 不允许被物理回收/复用：
/// - Drop 触发的 free 只能 retire 入队；
/// - 直到 epoch end 才会真正 nodes.remove（进入 free list）。
#[test]
fn epoch_defers_reclaim_until_end() {
    let mut engine = new_slotmap_engine();

    // 先创建一个节点并拉取一次，确保节点进入图中且 token 已分配。
    let var = Var::new(1usize);
    let anchor = var.watch();
    let _value = engine.get(&anchor);

    // 进入 epoch 后 Drop anchor，应该只 retire，不应立刻回收。
    let epoch = engine.enter_epoch();
    let token = anchor.token();
    drop(anchor);
    drop(var);

    let gc = engine.slotmap_gc_stats();
    assert!(
        gc.epoch_depth > 0,
        "进入 epoch 后 epoch_depth 应 > 0，实际为 {}",
        gc.epoch_depth
    );
    assert!(
        gc.retired_free > 0,
        "epoch 内 Drop 应把节点 retire 入队，retired_free 应 > 0"
    );
    assert_eq!(
        engine.token_audit_snapshot().last_deleted_token,
        None,
        "epoch 内不应发生 token 的物理回收（last_deleted_token 应保持 None）"
    );

    // 即便 stabilize 被调用，也不应在 epoch 内执行 reclaim。
    engine.stabilize();
    assert_eq!(
        engine.token_audit_snapshot().last_deleted_token,
        None,
        "epoch 内 stabilize 不应触发 reclaim（last_deleted_token 仍应为 None）"
    );

    // 退出 epoch 后才允许真正回收。
    drop(epoch);
    let gc2 = engine.slotmap_gc_stats();
    assert_eq!(gc2.epoch_depth, 0, "epoch end 后 depth 应归零");
    assert_eq!(gc2.retired_free, 0, "epoch end 后 retired 队列应被 flush");
    assert!(
        engine.token_audit_snapshot().last_deleted_token.is_some(),
        "epoch end 后应发生 token 回收（last_deleted_token 应变为 Some）"
    );

    // 保留 token 只是为了避免未使用告警，同时也能帮助调试。
    let _keep = token.raw_token();
}

/// 嵌套 epoch（depth>1）时，必须等最外层 guard drop 才能 flush_retired。
#[test]
fn nested_epoch_does_not_flush_early() {
    let mut engine = new_slotmap_engine();

    let var = Var::new(10usize);
    let anchor = var.watch();
    let _value = engine.get(&anchor);

    let outer = engine.enter_epoch();
    let inner = engine.enter_epoch();
    drop(anchor);
    drop(var);

    // 退出内层 epoch，不应触发 reclaim。
    drop(inner);
    assert_eq!(
        engine.token_audit_snapshot().last_deleted_token,
        None,
        "仍处于外层 epoch 时，不应提前 reclaim"
    );

    // 退出外层 epoch 才允许 reclaim。
    drop(outer);
    assert!(
        engine.token_audit_snapshot().last_deleted_token.is_some(),
        "最外层 epoch 结束后应触发 reclaim"
    );
}

/// epoch 内 retire 的节点不应进入 free list，因此 insert 不应复用其 ptr。
#[test]
fn epoch_retired_nodes_are_not_reused_by_insert() {
    let mut engine = new_slotmap_engine();

    let var1 = Var::new(1usize);
    let anchor1 = var1.watch();
    let _value = engine.get(&anchor1);
    let token1 = anchor1.token();

    let epoch = engine.enter_epoch();
    drop(anchor1);
    drop(var1);

    // epoch 期间创建新节点：ptr 不应复用已 retire 的 ptr。
    let var2 = Var::new(2usize);
    let anchor2 = var2.watch();
    let _value2 = engine.get(&anchor2);
    let token2 = anchor2.token();

    assert_ne!(
        token1.ptr, token2.ptr,
        "epoch 内 retire 的节点不应被 insert 复用（否则会产生读窗口内 token 失效）"
    );

    drop(epoch);
}
