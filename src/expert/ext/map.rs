use crate::expert::{
    Anchor, AnchorHandle, AnchorInner, Engine, OutputContext, Poll, UpdateContext,
};
use std::panic::Location;

pub struct Map<A, F, Out> {
    pub(super) f: F,
    pub(super) output: Option<Out>,
    pub(super) output_stale: bool,
    /// 一旦检测到依赖 token 已失效（PendingInvalidToken），就进入“冻结降级”模式：
    /// - 若已有旧输出：保持旧输出不变，并停止响应 dirty，避免重复 request 失效 token 刷屏/自旋。
    /// - 若无旧输出：将 PendingInvalidToken 向上传递，交由引擎 panic 暴露根因。
    pub(super) degraded_on_invalid: bool,
    pub(super) anchors: A,
    pub(super) location: &'static Location<'static>,
}
//TODO Out: Eq  case for vector eg
macro_rules! impl_tuple_map {
    ($([$output_type:ident, $num:tt])+) => {
        impl<$($output_type,)+ E, F, Out> AnchorInner<E> for
            Map<($(Anchor<$output_type, E>,)+), F, Out>
        where
            F: for<'any> FnMut($(&'any $output_type),+) -> Out,
            Out: PartialEq + 'static,
            $(
                $output_type: 'static,
            )+
            E: Engine,
        {
            type Output = Out;
            fn dirty(&mut self, edge:  &<E::AnchorHandle as AnchorHandle>::Token) {
                // 已进入降级冻结：不再响应任何 dirty，等待上层 drop/拆树。
                if self.degraded_on_invalid {
                    return;
                }
                if self.output_stale {
                    return;
                }
                // self.output_stale = true;
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
                    return self
                        .output
                        .as_ref()
                        .map(|_| Poll::Unchanged)
                        .unwrap_or(Poll::PendingInvalidToken);
                }

                if !self.output_stale && self.output.is_some() {
                    return Poll::Unchanged;
                }

                if emg_debug_env::bool_lenient("ANCHORS_DEBUG_MAP") {
                    println!(
                        "MAP poll output_stale={} has_output={} loc={:?}",
                        self.output_stale,
                        self.output.is_some(),
                        self.location
                    );
                }

                let mut found_pending = false;
                let mut found_invalid = false;
                let mut found_updated = false;
                #[cfg(debug_assertions)]
                let debug_degrade =
                    emg_debug_env::bool_lenient("ANCHORS_DEBUG_DEGRADE_ON_INVALID");
                #[cfg(debug_assertions)]
                let mut invalid_inputs: Option<
                    Vec<<<E as Engine>::AnchorHandle as AnchorHandle>::Token>,
                > = if debug_degrade {
                    Some(Vec::new())
                } else {
                    None
                };

                $(
                    let poll = ctx.request(&self.anchors.$num, true);
                    if poll.is_waiting() {
                        found_pending = true;
                    } else if poll.is_invalid_token() {
                        found_invalid = true;
                        #[cfg(debug_assertions)]
                        if let Some(v) = invalid_inputs.as_mut() {
                            v.push(self.anchors.$num.token());
                        }
                    } else if poll == Poll::Updated {
                        found_updated = true;
                    }
                )+

                ////////////////////////////////////////////////////////////////////////////////
                // NOTE:
                // - PendingInvalidToken 表示依赖 token 已失效（节点已被 GC/free），不会再变 Ready。
                // - 对于 map：
                //   - 若已有旧输出：直接冻结旧输出，并停止后续 dirty 响应，避免 stabilize 自旋或刷屏。
                //   - 若无旧输出：无法降级，必须向上传递 PendingInvalidToken，让引擎 panic 暴露根因。
                ////////////////////////////////////////////////////////////////////////////////
                if found_invalid {
                    if self.output.is_some() {
                        self.output_stale = false;
                        self.degraded_on_invalid = true;
                        #[cfg(debug_assertions)]
                        if debug_degrade {
                            eprintln!(
                                "[anchors][degrade_on_invalid] op=map loc={:?} out_type={} invalid_inputs={:?}",
                                self.location,
                                std::any::type_name::<Out>(),
                                invalid_inputs.unwrap_or_default(),
                            );
                        }
                        return Poll::Unchanged;
                    }
                    return Poll::PendingInvalidToken;
                }

                if found_pending {
                    return Poll::Pending;
                }

                self.output_stale = false;

                if self.output.is_none() || found_updated {
                    let new_val = Some((self.f)($(ctx.get(&self.anchors.$num)),+));
                    if new_val != self.output {
                        if emg_debug_env::bool_lenient("ANCHORS_DEBUG_MAP") {
                            println!(
                                "MAP 更新 loc={:?} type={} (值已变更)",
                                self.location,
                                std::any::type_name::<Out>(),
                            );
                        }
                        self.output = new_val;
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
                self.output
                    .as_ref()
                    .expect("output called on Map before value was calculated")
            }

            fn debug_location(&self) -> Option<(&'static str, &'static Location<'static>)> {
                Some(("map", self.location))
            }
        }
    }
}

impl_tuple_map! {
    [O0, 0]
}

impl_tuple_map! {
    [O0, 0]
    [O1, 1]
}

impl_tuple_map! {
    [O0, 0]
    [O1, 1]
    [O2, 2]
}

impl_tuple_map! {
    [O0, 0]
    [O1, 1]
    [O2, 2]
    [O3, 3]
}

impl_tuple_map! {
    [O0, 0]
    [O1, 1]
    [O2, 2]
    [O3, 3]
    [O4, 4]
}

impl_tuple_map! {
    [O0, 0]
    [O1, 1]
    [O2, 2]
    [O3, 3]
    [O4, 4]
    [O5, 5]
}

impl_tuple_map! {
    [O0, 0]
    [O1, 1]
    [O2, 2]
    [O3, 3]
    [O4, 4]
    [O5, 5]
    [O6, 6]
}

impl_tuple_map! {
    [O0, 0]
    [O1, 1]
    [O2, 2]
    [O3, 3]
    [O4, 4]
    [O5, 5]
    [O6, 6]
    [O7, 7]
}

impl_tuple_map! {
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
    /// 单测：当依赖返回 PendingInvalidToken 且已存在旧输出时，map 应冻结旧输出并返回 Unchanged，
    /// 同时进入“降级冻结”模式，避免后续重复 request 失效 token 导致刷屏/自旋。
    /// ////////////////////////////////////////////////////////////////////////////
    #[test]
    fn map_should_degrade_on_pending_invalid_token_when_has_old_output() {
        use crate::singlethread::Engine;

        // 初始化默认 mounter（Anchor::constant 依赖 Engine::new）
        let _engine = Engine::new();
        let a: Anchor<u32, Engine> = Anchor::constant(1);

        struct PendingInvalidCtx {
            requests: usize,
        }

        impl crate::expert::UpdateContext for PendingInvalidCtx {
            type Engine = crate::singlethread::Engine;

            fn get<'out, 'slf, O: 'static>(&'slf self, _anchor: &Anchor<O, Self::Engine>) -> &'out O
            where
                'slf: 'out,
            {
                panic!("map 在 PendingInvalidToken 分支不应调用 get()");
            }

            fn request<O: 'static>(
                &mut self,
                _anchor: &Anchor<O, Self::Engine>,
                _necessary: bool,
            ) -> Poll {
                self.requests += 1;
                Poll::PendingInvalidToken
            }

            fn unrequest<O: 'static>(&mut self, _anchor: &Anchor<O, Self::Engine>) {}

            fn dirty_handle(&mut self) -> <Self::Engine as crate::expert::Engine>::DirtyHandle {
                panic!("map 在 PendingInvalidToken 分支不应申请 dirty_handle()");
            }
        }

        let mut mapped: Map<(Anchor<u32, Engine>,), _, u32> = Map {
            anchors: (a,),
            f: |_x: &u32| -> u32 {
                panic!("PendingInvalidToken 时不应执行 f（因为依赖不可用）");
            },
            output: Some(123),
            output_stale: true,
            degraded_on_invalid: false,
            location: std::panic::Location::caller(),
        };

        let mut ctx = PendingInvalidCtx { requests: 0 };

        let poll_1 = mapped.poll_updated(&mut ctx);
        assert_eq!(Poll::Unchanged, poll_1);
        assert!(mapped.degraded_on_invalid);
        assert!(!mapped.output_stale);
        assert_eq!(Some(123), mapped.output);
        assert_eq!(1, ctx.requests);

        // 再次 poll：应直接走冻结快路径，不再 request。
        let poll_2 = mapped.poll_updated(&mut ctx);
        assert_eq!(Poll::Unchanged, poll_2);
        assert_eq!(1, ctx.requests, "降级冻结后不应再次 request 失效 token");
    }

    /// ////////////////////////////////////////////////////////////////////////////
    /// 单测：当依赖返回 PendingInvalidToken 且不存在旧输出时，map 必须向上传播
    /// PendingInvalidToken（由引擎 panic 暴露根因），不能把 invalid 当作普通 Pending。
    /// ////////////////////////////////////////////////////////////////////////////
    #[test]
    fn map_should_propagate_pending_invalid_token_when_no_old_output() {
        use crate::singlethread::Engine;

        let _engine = Engine::new();
        let a: Anchor<u32, Engine> = Anchor::constant(1);

        struct PendingInvalidCtx;

        impl crate::expert::UpdateContext for PendingInvalidCtx {
            type Engine = crate::singlethread::Engine;

            fn get<'out, 'slf, O: 'static>(&'slf self, _anchor: &Anchor<O, Self::Engine>) -> &'out O
            where
                'slf: 'out,
            {
                panic!("map 在 PendingInvalidToken 分支不应调用 get()");
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
                panic!("map 在 PendingInvalidToken 分支不应申请 dirty_handle()");
            }
        }

        let mut mapped: Map<(Anchor<u32, Engine>,), _, u32> = Map {
            anchors: (a,),
            f: |_x: &u32| -> u32 {
                panic!("PendingInvalidToken 时不应执行 f（因为依赖不可用）");
            },
            output: None,
            output_stale: true,
            degraded_on_invalid: false,
            location: std::panic::Location::caller(),
        };

        let mut ctx = PendingInvalidCtx;
        let poll = mapped.poll_updated(&mut ctx);
        assert_eq!(Poll::PendingInvalidToken, poll);
        assert!(
            !mapped.degraded_on_invalid,
            "无旧输出时不应进入降级冻结；应直接向上传播 invalid"
        );
    }
}
