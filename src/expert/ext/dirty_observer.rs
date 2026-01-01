use crate::expert::{
    Anchor, AnchorHandle, AnchorInner, Engine, OutputContext, Poll, UpdateContext,
};
use std::panic::Location;

/// DirtyObserverAnchor
///
/// 设计目的：
/// - 只要“上游发生过脏传播（dirty）”，就推进一个单调递增版本号；
/// - 不关心输入 Anchor 的输出值是否真的变化（允许假阳性）；
/// - 典型用途：用一个很便宜的版本号，去 gate 下游昂贵操作（如 Snapshot 重建、命中索引刷新）。
///
/// 关键语义（与 UpdateObserverAnchor 的区别）：
/// - UpdateObserver：基于 `Poll::Updated`，只在“输入输出真的变了”时推进版本号；
/// - DirtyObserver：基于 `dirty()` 回调，只要被标脏就推进版本号，即使输入最后计算为 Unchanged。
///
/// 注意：
/// - DirtyObserver 仍需要在 `poll_updated` 中对输入执行一次 `request`，用于：
///   1) 重新建立 clean_parent 链路（因为上游 Updated 时会 drain 父链路）；
///   2) 确保依赖关系在下一次上游更新时仍能正确传播 dirty。
/// - 如果你把 DirtyObserver 直接挂在一个“很重且可能处于 Not Ready”的派生 Anchor 上，
///   request 可能会触发该派生 Anchor 的重算；想要避免重算，请优先观察更上游的 StateVar/Var。
pub struct DirtyObserverAnchor<A> {
    /// 被观察的输入 Anchor 元组。
    pub(super) anchors: A,
    /// 版本号：单调递增，仅在观察到 dirty（或首次计算）时推进。
    pub(super) version: u64,
    /// 是否需要推进版本号（由 dirty 或首次计算触发）。
    pub(super) output_stale: bool,
    /// 调试定位信息。
    pub(super) location: &'static Location<'static>,
}

macro_rules! impl_tuple_dirty_observer {
    ($([$output_type:ident, $num:tt])+) => {
        impl<$($output_type,)+ E> AnchorInner<E>
            for DirtyObserverAnchor<($(Anchor<$output_type, E>,)+)>
        where
            $(
                $output_type: 'static,
            )+
            E: Engine,
        {
            type Output = u64;

            fn dirty(&mut self, edge: &<E::AnchorHandle as AnchorHandle>::Token) {
                // 只要任一输入被标脏，就认为“本轮需要推进版本号”。
                // 这里不去区分到底哪个输入变了，因为我们只做 gating 信号。
                if self.output_stale {
                    return;
                }
                $(
                    if edge == &self.anchors.$num.data.token() {
                        self.output_stale = true;
                        return;
                    }
                )+
            }

            fn poll_updated<G: UpdateContext<Engine = E>>(&mut self, ctx: &mut G) -> Poll {
                // 首次计算：即使没收到 dirty，也要推进到基线版本，避免下游无法区分“未初始化”。
                let need_bump = self.output_stale || self.version == 0;
                if !need_bump {
                    return Poll::Unchanged;
                }

                if std::env::var("ANCHORS_DEBUG_DIRTY_OBSERVER")
                    .map(|v| v != "0")
                    .unwrap_or(false)
                {
                    println!(
                        "DIRTY_OBSERVER poll stale={} ver={} loc={:?}",
                        self.output_stale,
                        self.version,
                        self.location
                    );
                }

                // 重新 request 输入，用于重建 clean_parent 链路（不关心返回的 Poll）。
                // 这里选择 necessary=false，避免把输入提升为 necessary（否则会放大重算范围）。
                let mut found_pending = false;
                $(
                    match ctx.request(&self.anchors.$num, false) {
                        Poll::Pending | Poll::PendingDefer | Poll::PendingInvalidToken => {
                            found_pending = true;
                        }
                        Poll::Updated | Poll::Unchanged => {
                            // do nothing
                        }
                    }
                )+

                if found_pending {
                    // 输入尚未 Ready，等下一轮 stabilize 再尝试。
                    return Poll::Pending;
                }

                // 输入已 Ready，本轮完成；清掉 stale 并推进版本号。
                self.output_stale = false;
                self.version = self.version.saturating_add(1);
                Poll::Updated
            }

            fn output<'slf, 'out, G: OutputContext<'out, Engine = E>>(
                &'slf self,
                _ctx: &mut G,
            ) -> &'out Self::Output
            where
                'slf: 'out,
            {
                &self.version
            }

            fn debug_location(&self) -> Option<(&'static str, &'static Location<'static>)> {
                Some(("dirty_observer", self.location))
            }
        }
    }
}

impl_tuple_dirty_observer! {
    [O0, 0]
}

impl_tuple_dirty_observer! {
    [O0, 0]
    [O1, 1]
}

impl_tuple_dirty_observer! {
    [O0, 0]
    [O1, 1]
    [O2, 2]
}

impl_tuple_dirty_observer! {
    [O0, 0]
    [O1, 1]
    [O2, 2]
    [O3, 3]
}

impl_tuple_dirty_observer! {
    [O0, 0]
    [O1, 1]
    [O2, 2]
    [O3, 3]
    [O4, 4]
}

impl_tuple_dirty_observer! {
    [O0, 0]
    [O1, 1]
    [O2, 2]
    [O3, 3]
    [O4, 4]
    [O5, 5]
}

impl_tuple_dirty_observer! {
    [O0, 0]
    [O1, 1]
    [O2, 2]
    [O3, 3]
    [O4, 4]
    [O5, 5]
    [O6, 6]
}

impl_tuple_dirty_observer! {
    [O0, 0]
    [O1, 1]
    [O2, 2]
    [O3, 3]
    [O4, 4]
    [O5, 5]
    [O6, 6]
    [O7, 7]
}

impl_tuple_dirty_observer! {
    [O0, 0]
    [O1, 1]
    [O2, 2]
    [O3, 3]
    [O4, 4]
    [O5, 5]
    [O6, 6]
    [O7, 7]
    [O8, 8]
}

#[cfg(test)]
mod tests {
    use crate::{
        expert::MultiAnchor,
        singlethread::{Engine, Var},
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    static MAP_CALLS: AtomicUsize = AtomicUsize::new(0);
    static MAP_CALLS_0: AtomicUsize = AtomicUsize::new(0);

    /// DirtyObserver 的典型用法：观察“上游便宜的输入 Anchor”，
    /// 用版本号 gate 下游很重的 map/布局/快照逻辑。
    #[test]
    fn dirty_observer_can_gate_heavy_map() {
        MAP_CALLS_0.store(0, Ordering::Relaxed);
        MAP_CALLS.store(0, Ordering::Relaxed);
        let mut engine = Engine::new();

        let a = Var::new(1i32);
        let b = Var::new(2i32);

        let deps = (&a.watch(), &b.watch());

        let mapped_0 = deps.map(|a, b| {
            println!("map 0, called");
            MAP_CALLS_0.fetch_add(1, Ordering::Relaxed);
            (*a + 1, *b + 1)
        });

        // 很重的计算（模拟）：我们不希望仅仅为了“判断是否变化”就执行它。
        let mapped = mapped_0.map(|(a, b)| {
            println!("map 1, called");

            MAP_CALLS.fetch_add(1, Ordering::Relaxed);
            (*a + 1, *b + 1)
        });

        // 观察上游输入的脏传播：只要 a/b 任意发生过 dirty，就版本号 +1。
        let obs = deps.dirty_observer();

        println!("--------------------------------------");

        let v1 = engine.get(&obs);
        assert_eq!(v1, 1);
        assert_eq!(MAP_CALLS.load(Ordering::Relaxed), 0);
        assert_eq!(MAP_CALLS_0.load(Ordering::Relaxed), 0);
        println!("v:{}", v1);
        println!("--------------------------------------");

        // 无输入更新时，版本号不变，也不触发 map。
        let v2 = engine.get(&obs);
        assert_eq!(v2, v1);
        assert_eq!(MAP_CALLS.load(Ordering::Relaxed), 0);
        assert_eq!(MAP_CALLS_0.load(Ordering::Relaxed), 0);
        println!("v:{}", v2);
        println!("--------------------------------------");

        a.set(10);
        let v3 = engine.get(&obs);
        assert_eq!(v3, v1 + 1);
        assert_eq!(MAP_CALLS.load(Ordering::Relaxed), 0);
        assert_eq!(MAP_CALLS_0.load(Ordering::Relaxed), 0);
        println!("v:{}", v3);
        println!("--------------------------------------");

        b.set(20);
        let v4 = engine.get(&obs);
        assert_eq!(v4, v3 + 1);
        assert_eq!(MAP_CALLS.load(Ordering::Relaxed), 0);
        assert_eq!(MAP_CALLS_0.load(Ordering::Relaxed), 0);
        println!("v:{}", v4);
        println!("--------------------------------------");

        // 当你真正需要结果时再 get(mapped)，才会执行重计算。
        let _val = engine.get(&mapped);
        assert_eq!(MAP_CALLS.load(Ordering::Relaxed), 1);
        assert_eq!(MAP_CALLS_0.load(Ordering::Relaxed), 1);
        println!("val:{:?}", _val);
        println!("--------------------------------------");
    }
}
