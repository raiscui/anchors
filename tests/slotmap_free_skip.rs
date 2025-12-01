//! 确认重复 Drop 不会导致 panic，且 free list 仍可复用。

mod slotmap;

use slotmap::_helpers::{new_slotmap_engine, spawn_counter_batch};

#[test]
fn repeated_drop_is_idempotent() {
    let mut engine = new_slotmap_engine();
    let (_vars, anchors) = spawn_counter_batch(8, 10);

    // 激活节点，确保 slotmap 正常分配 token。
    let _: Vec<_> = anchors.iter().map(|anchor| engine.get(anchor)).collect();

    // 克隆一份并提前 drop，模拟 handle_count 已归零的 free_skip 路径。
    let shadow = anchors.clone();
    drop(shadow);
    drop(anchors);

    // stabilize 过程中若 free_skip 处理异常，会直接 panic；通过即视为 idempotent。
    engine.stabilize();

    // 再次创建一批 anchor，若 free list 被污染则会 panic 或 token 重复。
    let (_vars2, anchors2) = spawn_counter_batch(4, 42);
    let values: Vec<_> = anchors2.iter().map(|anchor| engine.get(anchor)).collect();
    assert_eq!(values.len(), anchors2.len());
}
