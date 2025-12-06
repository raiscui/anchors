use anchors::singlethread::{Anchor, AnchorToken, Engine, Var};

/// ════════════════════════════════════════════════════════════════════════
/// SlotmapStressConfig 统一描述压测的规模，方便多处复用。
#[derive(Debug, Clone, Copy)]
pub struct SlotmapStressConfig {
    /// 每一轮要创建多少个临时 Anchor。
    pub nodes_per_cycle: usize,
    /// 要执行多少轮创建→访问→Drop 的循环。
    pub rounds: usize,
    /// 初始取值基准，避免测试中所有 Var 拥有相同的默认值。
    pub base_value: usize,
}

impl Default for SlotmapStressConfig {
    fn default() -> Self {
        Self {
            nodes_per_cycle: 128,
            rounds: 4,
            base_value: 1,
        }
    }
}

/// ════════════════════════════════════════════════════════════════════════
/// SlotmapCycleSnapshot 记录每一轮的数值与 token, 方便未来 insta/snapshot 对比。
#[derive(Debug, Clone)]
pub struct SlotmapCycleSnapshot {
    /// Engine::get 拉取的瞬时数值，验证计算正确性。
    pub values: Vec<usize>,
    /// Anchor token 序列，用于检测单调递增与非复用。
    pub tokens: Vec<AnchorToken>,
}

/// ════════════════════════════════════════════════════════════════════════
/// 断言 anchors_slotmap feature 已启用，若未启用则直接 panic，避免错误地跑到 arena_graph 分支。
pub fn assert_slotmap_feature_enabled() {
    #[cfg(not(feature = "anchors_slotmap"))]
    {
        panic!("tests require --features anchors_slotmap");
    }
}

/// ════════════════════════════════════════════════════════════════════════
/// 创建带 slotmap feature 的 Engine，后续测试可直接调用。
pub fn new_slotmap_engine() -> Engine {
    assert_slotmap_feature_enabled();
    Engine::new()
}

/// ════════════════════════════════════════════════════════════════════════
/// 构建一批计数 Anchor，返回 (Vars, Anchors) 方便随后 Drop/Mutation。
pub fn spawn_counter_batch(batch: usize, seed: usize) -> (Vec<Var<usize>>, Vec<Anchor<usize>>) {
    let mut vars = Vec::with_capacity(batch);
    let mut anchors = Vec::with_capacity(batch);
    for idx in 0..batch {
        // 使用 seed+idx 让每个节点的初始值独一无二，帮助调试。
        let var = Var::new(seed + idx);
        let anchor = var.watch();
        vars.push(var);
        anchors.push(anchor);
    }
    (vars, anchors)
}

/// ════════════════════════════════════════════════════════════════════════
/// 根据 round 序号批量更新 Var，模拟真实场景的数值波动。
pub fn mutate_vars(vars: &[Var<usize>], round: usize) {
    for (offset, var) in vars.iter().enumerate() {
        // 更新时加入 round 偏移，触发 Engine::get 路径。
        var.set(round + offset);
    }
}

/// ════════════════════════════════════════════════════════════════════════
/// 收集 Anchor token 形成易于比较的 Vec, 供后续 insta 快照或断言使用。
pub fn collect_tokens<T>(anchors: &[Anchor<T>]) -> Vec<AnchorToken>
where
    T: 'static,
{
    anchors.iter().map(|anchor| anchor.token()).collect()
}

/// ════════════════════════════════════════════════════════════════════════
/// 执行多轮创建→访问→Drop 的完整流程，并返回每轮的快照。
pub fn run_creation_drop_cycles(
    engine: &mut Engine,
    cfg: SlotmapStressConfig,
) -> Vec<SlotmapCycleSnapshot> {
    assert_slotmap_feature_enabled();
    let mut snapshots = Vec::with_capacity(cfg.rounds);
    for round in 0..cfg.rounds {
        let (vars, anchors) = spawn_counter_batch(cfg.nodes_per_cycle, cfg.base_value + round);
        let values: Vec<usize> = anchors.iter().map(|anchor| engine.get(anchor)).collect();
        let tokens = collect_tokens(&anchors);
        snapshots.push(SlotmapCycleSnapshot { values, tokens });
        mutate_vars(&vars, round);
        // 离开作用域即可释放 anchors/vars，模拟 handle drop。
        drop(anchors);
        drop(vars);
        // 立即跑一轮 stabilize，将 pending_free 清空到 free list，避免累积导致内存峰值被放大。
        engine.stabilize();
    }
    snapshots
}

/// ════════════════════════════════════════════════════════════════════════
/// 将快照转换成可直接写入 insta 的 Vec<String> 格式，提升可读性。
pub fn summarize_snapshot_for_snapshots(snapshot: &SlotmapCycleSnapshot) -> Vec<String> {
    snapshot
        .tokens
        .iter()
        .zip(snapshot.values.iter())
        .map(|(token, value)| format!("token={}, value={value}", token.raw_token()))
        .collect()
}
