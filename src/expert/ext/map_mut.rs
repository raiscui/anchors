use crate::expert::AnchorHandle;
use crate::expert::{Anchor, AnchorInner, Engine, OutputContext, Poll, UpdateContext};
use std::panic::Location;

pub struct MapMut<A, F, Out> {
    pub(super) f: F,
    pub(super) output: Out,
    pub(super) output_stale: bool,
    /// 一旦检测到依赖 token 已失效（PendingInvalidToken），就进入“冻结降级”模式：
    /// - 保持旧输出不变
    /// - 不再响应后续 dirty（避免其他依赖变化导致反复 request 失效 token，刷屏/自旋）
    pub(super) degraded_on_invalid: bool,
    pub(super) anchors: A,
    pub(super) location: &'static Location<'static>,
}

macro_rules! impl_tuple_map_mut {
    ($([$output_type:ident, $num:tt])+) => {
        impl<$($output_type,)+ E, F, Out> AnchorInner<E> for
            MapMut<($(Anchor<$output_type, E>,)+), F, Out>
        where
            F: for<'any> FnMut(&'any mut Out, $(&'any $output_type),+) -> bool,
            Out: PartialEq + 'static,
            $(
                $output_type: 'static,
            )+
            E: Engine,
        {
            type Output = Out;
            fn dirty(&mut self, edge:  &<E::AnchorHandle as crate::expert::AnchorHandle>::Token) {
                // 已进入降级冻结：不再响应任何 dirty，等待上层 drop/拆树。
                if self.degraded_on_invalid {
                    return;
                }
                // self.output_stale = true;
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
            fn poll_updated<G: UpdateContext<Engine=E>>(
                &mut self,
                ctx: &mut G,
            ) -> Poll {
                // 已进入降级冻结：保持旧输出，避免重复 request 失效 token。
                if self.degraded_on_invalid {
                    self.output_stale = false;
                    return Poll::Unchanged;
                }
                if !self.output_stale {
                    return Poll::Unchanged;
                }

                let mut found_pending = false;
                let mut found_invalid = false;
                let mut found_updated = false;

                $(
                    let poll = ctx.request(&self.anchors.$num, true);
                    if poll.is_waiting() {
                        found_pending = true;
                    } else if poll.is_invalid_token() {
                        found_invalid = true;
                    } else if matches!(poll, Poll::Updated) {
                        found_updated = true;
                    }
                )+

                ////////////////////////////////////////////////////////////////////////////////
                // NOTE:
                // - PendingInvalidToken 在这里被视为“可降级”的 Pending：
                //   表示依赖 token 已失效（节点已被 GC/free）。
                // - 对于 map_mut：我们始终持有一个可用的旧输出，因此直接保持旧输出即可，
                //   避免把 Pending 继续向上传播导致 stabilize 自旋或 panic。
                ////////////////////////////////////////////////////////////////////////////////
                if found_invalid {
                    self.output_stale = false;
                    self.degraded_on_invalid = true;
                    return Poll::Unchanged;
                }

                if found_pending {
                    return Poll::Pending;
                }

                self.output_stale = false;

                if found_updated {
                    let did_update = (self.f)(&mut self.output, $(&ctx.get(&self.anchors.$num)),+);
                    if did_update {
                        return Poll::Updated
                    }
                }
                Poll::Unchanged
            }
            fn output<'slf, 'out, G: OutputContext<'out, Engine=E>>(
                &'slf self,
                _ctx: &mut G,
            ) -> &'out Self::Output
            where
                'slf: 'out,
            {
                &self.output
            }

            fn debug_location(&self) -> Option<(&'static str, &'static Location<'static>)> {
                Some(("map", self.location))
            }
        }
    }
}

impl_tuple_map_mut! {
    [O0, 0]
}

impl_tuple_map_mut! {
    [O0, 0]
    [O1, 1]
}

impl_tuple_map_mut! {
    [O0, 0]
    [O1, 1]
    [O2, 2]
}

impl_tuple_map_mut! {
    [O0, 0]
    [O1, 1]
    [O2, 2]
    [O3, 3]
}

impl_tuple_map_mut! {
    [O0, 0]
    [O1, 1]
    [O2, 2]
    [O3, 3]
    [O4, 4]
}

impl_tuple_map_mut! {
    [O0, 0]
    [O1, 1]
    [O2, 2]
    [O3, 3]
    [O4, 4]
    [O5, 5]
}

impl_tuple_map_mut! {
    [O0, 0]
    [O1, 1]
    [O2, 2]
    [O3, 3]
    [O4, 4]
    [O5, 5]
    [O6, 6]
}

impl_tuple_map_mut! {
    [O0, 0]
    [O1, 1]
    [O2, 2]
    [O3, 3]
    [O4, 4]
    [O5, 5]
    [O6, 6]
    [O7, 7]
}

impl_tuple_map_mut! {
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
    use super::*;

    /// ////////////////////////////////////////////////////////////////////////////
    /// 单测：当依赖返回 PendingInvalidToken 时，map_mut 应保持旧输出并返回 Unchanged。
    ///
    /// 背景：
    /// - 拆树期可能出现“失效 token request”，在 EngineContextMut::request 中会转成 PendingDefer；
    /// - map_mut 作为缓存型节点，应优先保持旧输出，避免把 Pending 继续向上传播导致 panic/自旋。
    /// ////////////////////////////////////////////////////////////////////////////
    #[test]
    fn map_mut_should_degrade_on_pending_invalid_token() {
        use crate::singlethread::Engine;

        // 初始化默认 mounter（Anchor::constant 依赖 Engine::new）
        let _engine = Engine::new();

        let a: Anchor<u32, Engine> = Anchor::constant(1);
        let b: Anchor<u32, Engine> = Anchor::constant(2);

        struct PendingDeferOnlyCtx;

        impl crate::expert::UpdateContext for PendingDeferOnlyCtx {
            type Engine = crate::singlethread::Engine;

            fn get<'out, 'slf, O: 'static>(&'slf self, _anchor: &Anchor<O, Self::Engine>) -> &'out O
            where
                'slf: 'out,
            {
                panic!("map_mut 在 PendingInvalidToken 分支不应调用 get()");
            }

            fn request<O: 'static>(
                &mut self,
                _anchor: &Anchor<O, Self::Engine>,
                _necessary: bool,
            ) -> Poll {
                Poll::PendingInvalidToken
            }

            fn unrequest<O: 'static>(&mut self, _anchor: &Anchor<O, Self::Engine>) {}

            fn dirty_handle(&mut self) -> <Self::Engine as crate::expert::Engine>::DirtyHandle {
                panic!("map_mut 在 PendingInvalidToken 分支不应申请 dirty_handle()");
            }
        }

        let mut mapped: MapMut<(Anchor<u32, Engine>, Anchor<u32, Engine>), _, u32> = MapMut {
            anchors: (a, b),
            output: 123,
            output_stale: true,
            degraded_on_invalid: false,
            location: std::panic::Location::caller(),
            f: |_out: &mut u32, _x: &u32, _y: &u32| -> bool {
                panic!("PendingInvalidToken 时不应执行 f（因为依赖不可用）");
            },
        };

        let mut ctx = PendingDeferOnlyCtx;
        let poll = mapped.poll_updated(&mut ctx);
        assert_eq!(Poll::Unchanged, poll);
        assert!(
            !mapped.output_stale,
            "应当清除 output_stale，避免无意义重复尝试"
        );
        assert_eq!(123, mapped.output);
    }
}
