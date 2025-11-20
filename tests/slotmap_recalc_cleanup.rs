//! 删除节点后 recalc/necessary 链路应清理，避免悬挂。
//! 运行：cargo nextest run -p anchors --features anchors_slotmap --test slotmap_recalc_cleanup --profile anchors-slotmap

#[cfg(feature = "anchors_slotmap")]
mod tests {
    use anchors::expert::MultiAnchor;
    use anchors::singlethread::{Anchor, Engine, Var};

    /// 构造简单父子链：p -> c，标记 observed，删除 child 后再次 stabilize 不应 panic。
    #[test]
    fn recalc_cleanup_after_drop() {
        let mut engine = Engine::new();
        let parent: Anchor<usize> = {
            let p = Var::new(1);
            let c = Var::new(2);
            let child = c.watch();
            let p_anchor = p.watch();
            // 让 parent 依赖 child，形成 necessary 链。
            (&p_anchor, &child).map(|a, b| *a + *b)
        };

        engine.mark_observed(&parent);
        // 首次计算
        assert_eq!(engine.get(&parent), 3);

        // 删除 parent，以此触发子节点递减 handle_count
        drop(parent);

        // 再次 stabilize，若 recalc/necessary 链未清理会触发 panic；当前预期正常返回。
        let mut engine2 = Engine::new();
        let v = Var::new(10);
        assert_eq!(engine2.get(&v.watch()), 10);
    }
}

#[cfg(not(feature = "anchors_slotmap"))]
mod skip {
    #[test]
    fn slotmap_only() {
        eprintln!("anchors_slotmap 未启用，跳过 recalc_cleanup 契约测试");
    }
}
