use crate::expert::{
    Anchor, AnchorHandle, AnchorInner, Engine, OutputContext, Poll, UpdateContext,
};
use std::panic::Location;

/// UpdateObserverAnchor
///
/// 设计目的：
/// - 观察一组输入 Anchor 的“更新情况”，并把结果压缩成一个单调递增的版本号。
/// - 下游只需要比较该版本号是否变化，就能判断“本轮输入是否发生过 Updated”，
///   从而决定是否执行重建 Snapshot、更新命中索引等重活。
///
/// 语义：
/// - 首次 poll 一定会把版本号从 0 推进到 1，确保下游能拿到初始基线。
/// - 之后每当任一输入 Anchor 在 poll 中返回 Poll::Updated，版本号 +1。
/// - 如果输入都为 Unchanged，则版本号不变，返回 Poll::Unchanged。
pub struct UpdateObserverAnchor<A> {
    /// 被观察的输入 Anchor 元组。
    pub(super) anchors: A,
    /// 版本号：单调递增，仅在观察到 Updated 时推进。
    pub(super) version: u64,
    /// 输出是否需要重新检查（由 dirty 或首次计算触发）。
    pub(super) output_stale: bool,
    /// 调试定位信息。
    pub(super) location: &'static Location<'static>,
}

macro_rules! impl_tuple_update_observer {
    ($([$output_type:ident, $num:tt])+) => {
        impl<$($output_type,)+ E> AnchorInner<E>
            for UpdateObserverAnchor<($(Anchor<$output_type, E>,)+)>
        where
            $(
                $output_type: 'static,
            )+
            E: Engine,
        {
            type Output = u64;

            fn dirty(&mut self, edge: &<E::AnchorHandle as AnchorHandle>::Token) {
                // 只要任一输入被标脏，我们就把自己标成 stale，
                // 等下一次 poll_updated 时统一 request 输入并决定是否推进版本。
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
                // 没有 stale 表示上一轮检查后输入都没变，
                // 直接走 Unchanged 快路径，避免无意义 request。
                if !self.output_stale {
                    return Poll::Unchanged;
                }

                if std::env::var("ANCHORS_DEBUG_UPDATE_OBSERVER")
                    .map(|v| v != "0")
                    .unwrap_or(false)
                {
                    println!(
                        "UPDATE_OBSERVER poll stale={} ver={} loc={:?}",
                        self.output_stale,
                        self.version,
                        self.location
                    );
                }

                let mut found_pending = false;
                let mut found_updated = false;

                $(
                    match ctx.request(&self.anchors.$num, true) {
                        Poll::Pending | Poll::PendingDefer => {
                            found_pending = true;
                        }
                        Poll::Updated => {
                            found_updated = true;
                        }
                        Poll::Unchanged => {
                            // do nothing
                        }
                    }
                )+

                if found_pending {
                    return Poll::Pending;
                }

                // 所有输入已 Ready，本轮检查完成，清掉 stale。
                self.output_stale = false;

                // 首次计算或观察到 Updated 时推进版本号。
                if found_updated || self.version == 0 {
                    self.version = self.version.saturating_add(1);
                    return Poll::Updated;
                }

                Poll::Unchanged
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
                Some(("update_observer", self.location))
            }
        }
    }
}

impl_tuple_update_observer! {
    [O0, 0]
}

impl_tuple_update_observer! {
    [O0, 0]
    [O1, 1]
}

impl_tuple_update_observer! {
    [O0, 0]
    [O1, 1]
    [O2, 2]
}

impl_tuple_update_observer! {
    [O0, 0]
    [O1, 1]
    [O2, 2]
    [O3, 3]
}

impl_tuple_update_observer! {
    [O0, 0]
    [O1, 1]
    [O2, 2]
    [O3, 3]
    [O4, 4]
}

impl_tuple_update_observer! {
    [O0, 0]
    [O1, 1]
    [O2, 2]
    [O3, 3]
    [O4, 4]
    [O5, 5]
}

impl_tuple_update_observer! {
    [O0, 0]
    [O1, 1]
    [O2, 2]
    [O3, 3]
    [O4, 4]
    [O5, 5]
    [O6, 6]
}

impl_tuple_update_observer! {
    [O0, 0]
    [O1, 1]
    [O2, 2]
    [O3, 3]
    [O4, 4]
    [O5, 5]
    [O6, 6]
    [O7, 7]
}

impl_tuple_update_observer! {
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

    /// 观察器应该在首次读取时给出基线版本，
    /// 并且仅在输入 Updated 时单调递增。
    #[test]
    fn update_observer_increments_on_child_update() {
        let mut engine = Engine::new();

        let a = Var::new(1i32);
        let b = Var::new(2i32);

        let obs = (&a.watch(), &b.watch())
            .map(|a, b| {
                println!("Computing update observer for a={}, b={}", a, b);
                (a + 1, b + 1)
            })
            .update_observer();

        let v1 = engine.get(&obs);
        assert_eq!(v1, 1);

        // 无更新时版本号不变
        let v2 = engine.get(&obs);
        assert_eq!(v2, v1);

        a.set(10);
        let v3 = engine.get(&obs);
        assert_eq!(v3, v1 + 1);

        b.set(20);
        let v4 = engine.get(&obs);
        assert_eq!(v4, v3 + 1);
        println!("end");
        // 再次读取仍然保持不变
        let v5 = engine.get(&obs);
        assert_eq!(v5, v4);
    }
}
