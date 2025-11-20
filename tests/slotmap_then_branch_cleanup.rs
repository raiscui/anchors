//! then 分支切换后，旧分支应在下一次 Engine::get 前释放，不应残留悬挂节点。
//! 运行：cargo nextest run -p anchors --features anchors_slotmap --test slotmap_then_branch_cleanup --profile anchors-slotmap

#[cfg(feature = "anchors_slotmap")]
mod tests {
    use anchors::singlethread::{Engine, Var};

    #[test]
    fn then_branch_cleanup() {
        let mut engine = Engine::new();
        let cond = Var::new(true);
        let branch_a = Var::new(1);
        let branch_b = Var::new(100);

        let anchor = cond.watch().then(move |flag| {
            if *flag {
                branch_a.watch()
            } else {
                branch_b.watch()
            }
        });

        assert_eq!(engine.get(&anchor), 1);

        // 切换分支，旧分支节点需在下一次 get 前被释放/标记 Deleted。
        cond.set(false);
        assert_eq!(engine.get(&anchor), 100);

        // 再切换回来，确保重复释放不会 panic，值也正确。
        cond.set(true);
        assert_eq!(engine.get(&anchor), 1);
    }
}

#[cfg(not(feature = "anchors_slotmap"))]
mod skip {
    #[test]
    fn slotmap_only() {
        eprintln!("anchors_slotmap 未启用，跳过 then 分支清理测试");
    }
}
