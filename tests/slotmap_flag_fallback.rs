//! 确认禁用 anchors_slotmap 时仍使用旧存储（token 不递增、特性提示）。
//! 运行：cargo nextest run -p anchors --no-default-features --features generic_size_voa --test slotmap_flag_fallback

#[cfg(not(feature = "anchors_slotmap"))]
mod tests {
    use anchors::singlethread::{Engine, Var};

    #[test]
    fn fallback_arena_graph_token_stable() {
        let mut engine = Engine::new();
        let a = Var::new(1);
        let b = Var::new(2);
        let ta = a.watch().token();
        let tb = b.watch().token();
        // 在 arena_graph 下，同一 Graph2 内所有节点 token 应相同（graph_token），以区别 slotmap 递增行为。
        assert_eq!(ta.token, tb.token, "fallback 预期 token 相同，表示未启用 slotmap");
        assert_eq!(engine.get(&a.watch()), 1);
        assert_eq!(engine.get(&b.watch()), 2);
    }
}

#[cfg(feature = "anchors_slotmap")]
mod skip {
    #[test]
    fn slotmap_enabled_skip_fallback_contract() {
        eprintln!(
            "slotmap flag 已启用，此合同测试仅在 --no-default-features --features arena_graph 下运行"
        );
    }
}
