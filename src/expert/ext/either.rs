use tracing::debug;

/*
 * @Author: Rais
 * @Date: 2023-03-23 12:55:22
 * @LastEditTime: 2023-03-27 16:35:46
 * @LastEditors: Rais
 * @Description:
 */
use crate::expert::{
    Anchor, AnchorHandle, AnchorInner, Engine, OutputContext, Poll, UpdateContext, ValOrAnchor,
};
use std::panic::Location;

pub struct Either<A, Out, F, E: Engine> {
    pub(super) f: F,
    pub(super) either_anchor: Option<ValOrAnchor<Out, E>>,
    pub(super) output_stale: bool,
    /// 一旦检测到依赖 token 已失效（PendingInvalidToken），就进入“冻结降级”模式：
    /// - 若已有旧输出（either_anchor 已确定）：保持旧输出不变，并停止响应 dirty，避免重复 request 失效 token 刷屏/自旋。
    /// - 若无旧输出：将 PendingInvalidToken 向上传递，交由引擎 panic 暴露根因。
    pub(super) degraded_on_invalid: bool,
    pub(super) anchors: A,
    pub(super) location: &'static Location<'static>,
}

macro_rules! impl_tuple_either {
    ($([$output_type:ident, $num:tt])+) => {
        impl<$($output_type,)+ E, F, Out> AnchorInner<E> for
        Either<( $(Anchor<$output_type, E>,)+ ), Out, F, E>
        where
            F: for<'any> FnMut($(&'any $output_type),+) -> ValOrAnchor<Out, E>,
            Out: PartialEq + 'static,
            $(
                $output_type: 'static,
            )+
            E: Engine,
        {
            type Output = Out;

            fn dirty(&mut self, edge: &<E::AnchorHandle as AnchorHandle>::Token) {
                // 已进入降级冻结：不再响应任何 dirty，等待上层 drop/拆树。
                if self.degraded_on_invalid {
                    return;
                }
                if self.output_stale {
                    return;
                }
                $(
                    // only invalidate either_anchor if one of the lhs anchors is invalidated
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
                let mut poll = Poll::Unchanged;
                let mut output_anchor_poll: Option<Poll> = None;

                // 已进入降级冻结：保持旧输出，避免重复 request 失效 token。
                if self.degraded_on_invalid {
                    self.output_stale = false;
                    return self
                        .either_anchor
                        .as_ref()
                        .map(|_| Poll::Unchanged)
                        .unwrap_or(Poll::PendingInvalidToken);
                }

                if self.either_anchor.is_none() || self.output_stale {
                    let mut found_pending = false;
                    let mut found_invalid = false;
                    let mut found_updated = false;

                    $(
                        match ctx.request(&self.anchors.$num, true) {
                            Poll::Pending | Poll::PendingDefer => {
                                found_pending = true;
                            }
                            Poll::PendingInvalidToken => {
                                found_invalid = true;
                            }
                            Poll::Updated => {
                                found_updated = true;
                            }
                            Poll::Unchanged => {
                                // do nothing
                            }
                        }
                    )+

                    ////////////////////////////////////////////////////////////////////////////////
                    // NOTE:
                    // - PendingInvalidToken 表示依赖 token 已失效（节点已被 GC/free），不会再变 Ready。
                    // - 对于 either：
                    //   - 若已有旧输出（either_anchor 已确定）：直接冻结旧输出，并停止后续 dirty 响应，避免 stabilize 自旋或刷屏。
                    //   - 若无旧输出：无法降级，必须向上传递 PendingInvalidToken，让引擎 panic 暴露根因。
                    ////////////////////////////////////////////////////////////////////////////////
                    if found_invalid {
                        if self.either_anchor.is_some() {
                            self.output_stale = false;
                            self.degraded_on_invalid = true;
                            return Poll::Unchanged;
                        }
                        return Poll::PendingInvalidToken;
                    }

                    if found_pending {
                        return Poll::Pending;
                    }

                    self.output_stale = false;

                    if self.either_anchor.is_none() || found_updated {

                        let new_either_anchor = (self.f)($(ctx.get(&self.anchors.$num)),+);

                        // ──────────────────────────────────────────────────────────────
                        // 切换器语义（either/var 同类）：
                        // - 若切换到的新 Anchor 是 invalid：回退到旧输出（若存在），避免 stabilize 自旋/刷屏；
                        // - 若无旧输出：向上传播 PendingInvalidToken，让引擎 panic 暴露根因。
                        // ──────────────────────────────────────────────────────────────
                        if let ValOrAnchor::Anchor(new_anchor) = &new_either_anchor {
                            let is_same_anchor = matches!(
                                self.either_anchor.as_ref(),
                                Some(ValOrAnchor::Anchor(old_anchor)) if old_anchor == new_anchor
                            );
                            if !is_same_anchor {
                                let new_poll = ctx.request(new_anchor, true);
                                match new_poll {
                                    Poll::PendingInvalidToken => {
                                        if self.either_anchor.is_some() {
                                            // 回退到旧输出：不更新 either_anchor。
                                            self.output_stale = false;
                                            return Poll::Unchanged;
                                        }
                                        return Poll::PendingInvalidToken;
                                    }
                                    Poll::Pending | Poll::PendingDefer => {
                                        // 切换到新输出并等待它 ready。
                                        if let Some(ValOrAnchor::Anchor(outdated_anchor)) =
                                            self.either_anchor.as_ref()
                                        {
                                            ctx.unrequest(outdated_anchor);
                                        }
                                        self.either_anchor = Some(new_either_anchor);
                                        return Poll::Pending;
                                    }
                                    ready_poll @ (Poll::Updated | Poll::Unchanged) => {
                                        output_anchor_poll = Some(ready_poll);
                                    }
                                }
                            }
                        }

                        match (&self.either_anchor, &new_either_anchor) {
                            (None, ValOrAnchor::Anchor(_)) => {
                                // poll = Poll::Updated;
                            }
                            (None, ValOrAnchor::Val(_)) => {
                                poll = Poll::Updated;
                            }

                            (Some(ValOrAnchor::Val(_)), ValOrAnchor::Anchor(_)) => {
                                poll = Poll::Updated;
                            }
                            (Some(ValOrAnchor::Val(a)), ValOrAnchor::Val(b)) => {
                                if a != b {
                                    poll = Poll::Updated;
                                }
                            }
                            (Some(ValOrAnchor::Anchor(outdated_anchor)), ValOrAnchor::Val(_)) => {
                                debug!("either anchor - val");

                                ctx.unrequest(outdated_anchor);
                                poll = Poll::Updated;
                            }

                            (
                                Some(ValOrAnchor::Anchor(outdated_anchor)),
                                ValOrAnchor::Anchor(new_anchor),
                            ) => {
                                debug!("either anchor - anchor");

                                if outdated_anchor != new_anchor {
                                    ctx.unrequest(outdated_anchor);
                                    poll = Poll::Updated;
                                }
                            }
                        }
                        self.either_anchor = Some(new_either_anchor);
                    }
                }

                match self.either_anchor.as_ref().unwrap() {
                    ValOrAnchor::Val(_) => poll,
                    ValOrAnchor::Anchor(an) => match output_anchor_poll.unwrap_or_else(|| ctx.request(an, true)) {
                        Poll::Updated => Poll::Updated,
                        Poll::Unchanged => poll,
                        Poll::Pending | Poll::PendingDefer => Poll::Pending,
                        Poll::PendingInvalidToken => Poll::PendingInvalidToken,
                    },
                }
            }
            fn output<'slf, 'out, G: OutputContext<'out, Engine=E>>(
                &'slf self,
                ctx: &mut G,
            ) -> &'out Self::Output
            where
                'slf: 'out,
            {
                match self.either_anchor.as_ref().unwrap() {
                    ValOrAnchor::Val(v) => v,
                    ValOrAnchor::Anchor(an) => ctx.get(an),
                }
            }

            fn debug_location(&self) -> Option<(&'static str, &'static Location<'static>)> {
                Some(("either", self.location))
            }
        }
    }
}

impl_tuple_either! {
    [O0, 0]
}

impl_tuple_either! {
    [O0, 0]
    [O1, 1]
}
impl_tuple_either! {
    [O0, 0]
    [O1, 1]
    [O2, 2]
}

impl_tuple_either! {
    [O0, 0]
    [O1, 1]
    [O2, 2]
    [O3, 3]
}

impl_tuple_either! {
    [O0, 0]
    [O1, 1]
    [O2, 2]
    [O3, 3]
    [O4, 4]
}

impl_tuple_either! {
    [O0, 0]
    [O1, 1]
    [O2, 2]
    [O3, 3]
    [O4, 4]
    [O5, 5]
}

impl_tuple_either! {
    [O0, 0]
    [O1, 1]
    [O2, 2]
    [O3, 3]
    [O4, 4]
    [O5, 5]
    [O6, 6]
}

impl_tuple_either! {
    [O0, 0]
    [O1, 1]
    [O2, 2]
    [O3, 3]
    [O4, 4]
    [O5, 5]
    [O6, 6]
    [O7, 7]
}

impl_tuple_either! {
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
    use tracing::debug;

    use crate::{
        expert::MultiAnchor,
        singlethread::{Engine, Var, VarVOA},
    };

    #[test]
    fn either_tuple() {
        let mut engine = Engine::new();

        let a = VarVOA::new(1);
        let b = Var::new(2);
        let c = Var::new(3);
        let d = Var::new(4);

        let c2 = c.clone();
        let d2 = d.clone();

        let a2 = a.clone();
        let b2 = b.clone();

        let aw = (&a.watch(), &b.watch()).either(move |aa, bb| {
            if aa + bb < 3 {
                c2.watch().into()
            } else if aa + bb == 3 {
                100.into()
            } else {
                d2.watch().into()
            }
        });

        let awthen = aw.then(move |&x| if x > 3 { a2.watch() } else { b2.watch() });
        let awmap = aw.map(move |&x| x + 1);
        assert_eq!(engine.get(&aw), 100);
        assert_eq!(engine.get(&awthen), 1);
        assert_eq!(engine.get(&awmap), 101);

        a.set(2);
        assert_eq!(engine.get(&aw), 4);
        assert_eq!(engine.get(&awthen), 2);
        assert_eq!(engine.get(&awmap), 5);

        b.set(0);
        assert_eq!(engine.get(&aw), 3);
        assert_eq!(engine.get(&awthen), 0);
        assert_eq!(engine.get(&awmap), 4);

        c.set(10);
        assert_eq!(engine.get(&aw), 10);
        assert_eq!(engine.get(&awthen), 2);
        assert_eq!(engine.get(&awmap), 11);
    }

    #[test]
    fn aaaxx2() {
        let mut engine = Engine::new();
        let a = VarVOA::new(1);
        let c = Var::new(22);
        let d = Var::new(-1);

        let b = Var::new(9);
        let bw = b.watch();
        let _bw2 = bw.clone();

        let w = a.watch().either(move |&x| {
            debug!("in either  ,  x:{x}");
            if x >= 2 { bw.clone().into() } else { x.into() }
        });
        let ww = w.map(|x| {
            debug!("in map  ww,  x:{x}");
            x + 1
        });

        assert_eq!(engine.get(&w), 1);
        assert_eq!(engine.get(&ww), 2);
        a.set(-1);
        assert_eq!(engine.get(&w), -1);
        assert_eq!(engine.get(&ww), 0);
        a.set(3);
        assert_eq!(engine.get(&w), 9);
        assert_eq!(engine.get(&ww), 10);
        b.set(11);
        assert_eq!(engine.get(&w), 11);
        assert_eq!(engine.get(&ww), 12);
        a.set(-2);
        assert_eq!(engine.get(&w), -2);
        assert_eq!(engine.get(&ww), -1);

        debug!(".....................");
        a.set(c.watch());
        // a.set(_bw2);
        debug!("a seted .....................");

        // debug!("a:{}", engine.get(a.get().as_anchor().unwrap()));
        // debug!("a:{}", engine.get(&a.watch()));
        debug!("w:{}", engine.get(&w));
        assert_eq!(engine.get(&w), 11);
        assert_eq!(engine.get(&ww), 12);

        a.set(d.watch());
        assert_eq!(engine.get(&w), -1);
        assert_eq!(engine.get(&ww), 0);

        a.set(c.watch());
        assert_eq!(engine.get(&w), 11);
        assert_eq!(engine.get(&ww), 12);
    }

    #[test]
    fn aaaxx() {
        let mut engine = Engine::new();
        let a = VarVOA::new(1);
        let _c = Var::new(22);
        let d = Var::new(-1);
        let b = Var::new(9);
        let bw = b.watch();
        let bw2 = bw.clone();

        let w = a.watch().either(move |&x| {
            debug!("in either  ,  x:{x}");
            if x >= 2 { bw.clone().into() } else { x.into() }
        });

        assert_eq!(engine.get(&w), 1);
        a.set(-1);
        assert_eq!(engine.get(&w), -1);
        a.set(3);
        assert_eq!(engine.get(&w), 9);
        b.set(11);
        assert_eq!(engine.get(&w), 11);
        a.set(-2);
        assert_eq!(engine.get(&w), -2);
        debug!(".....................");
        // a.set(c.watch());
        a.set(bw2.clone());
        debug!("a seted .....................");

        // debug!("a:{}", engine.get(a.get().as_anchor().unwrap()));
        // debug!("a:{}", engine.get(&a.watch()));
        debug!("w:{}", engine.get(&w));
        assert_eq!(engine.get(&w), 11);

        a.set(d.watch());
        debug!("w:{}", engine.get(&w));
        assert_eq!(engine.get(&w), -1);

        a.set(bw2);
        debug!("a seted .....................");

        debug!("w:{}", engine.get(&w));
        assert_eq!(engine.get(&w), 11);
    }

    /// ////////////////////////////////////////////////////////////////////////////
    /// 单测：当依赖返回 PendingInvalidToken 且已存在旧输出时，either 应冻结旧输出并进入
    /// “降级冻结”模式，避免重复 request 失效 token 导致刷屏/自旋。
    /// ////////////////////////////////////////////////////////////////////////////
    #[test]
    fn either_should_degrade_on_pending_invalid_token_when_has_old_output() {
        use crate::expert::{Anchor, AnchorInner, Poll, ValOrAnchor};

        let _engine = Engine::new();
        let a: Anchor<u32, Engine> = Anchor::constant(1);

        struct PendingInvalidCtx {
            requests: usize,
        }

        impl crate::expert::UpdateContext for PendingInvalidCtx {
            type Engine = Engine;

            fn get<'out, 'slf, O: 'static>(&'slf self, _anchor: &Anchor<O, Self::Engine>) -> &'out O
            where
                'slf: 'out,
            {
                panic!("either 在 PendingInvalidToken 分支不应调用 get()");
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
                panic!("either 在 PendingInvalidToken 分支不应申请 dirty_handle()");
            }
        }

        let mut either: super::Either<(Anchor<u32, Engine>,), u32, _, Engine> = super::Either {
            anchors: (a,),
            f: |_x: &u32| -> ValOrAnchor<u32, Engine> {
                panic!("PendingInvalidToken 时不应执行 f（因为依赖不可用）");
            },
            either_anchor: Some(ValOrAnchor::Val(123)),
            output_stale: true,
            degraded_on_invalid: false,
            location: std::panic::Location::caller(),
        };

        let mut ctx = PendingInvalidCtx { requests: 0 };
        let poll_1 = AnchorInner::poll_updated(&mut either, &mut ctx);
        assert_eq!(Poll::Unchanged, poll_1);
        assert!(either.degraded_on_invalid);
        assert!(!either.output_stale);
        assert_eq!(1, ctx.requests);

        // 再次 poll：应直接走冻结快路径，不再 request。
        let poll_2 = AnchorInner::poll_updated(&mut either, &mut ctx);
        assert_eq!(Poll::Unchanged, poll_2);
        assert_eq!(1, ctx.requests, "降级冻结后不应再次 request 失效 token");
    }

    /// ////////////////////////////////////////////////////////////////////////////
    /// 单测：当依赖返回 PendingInvalidToken 且不存在旧输出（either_anchor 尚未确定）时，
    /// either 必须向上传播 PendingInvalidToken（由引擎 panic 暴露根因）。
    /// ////////////////////////////////////////////////////////////////////////////
    #[test]
    fn either_should_propagate_pending_invalid_token_when_no_old_output() {
        use crate::expert::{Anchor, AnchorInner, Poll, ValOrAnchor};

        let _engine = Engine::new();
        let a: Anchor<u32, Engine> = Anchor::constant(1);

        struct PendingInvalidCtx;

        impl crate::expert::UpdateContext for PendingInvalidCtx {
            type Engine = Engine;

            fn get<'out, 'slf, O: 'static>(&'slf self, _anchor: &Anchor<O, Self::Engine>) -> &'out O
            where
                'slf: 'out,
            {
                panic!("either 在 PendingInvalidToken 分支不应调用 get()");
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
                panic!("either 在 PendingInvalidToken 分支不应申请 dirty_handle()");
            }
        }

        let mut either: super::Either<(Anchor<u32, Engine>,), u32, _, Engine> = super::Either {
            anchors: (a,),
            f: |_x: &u32| -> ValOrAnchor<u32, Engine> {
                panic!("PendingInvalidToken 时不应执行 f（因为依赖不可用）");
            },
            either_anchor: None,
            output_stale: true,
            degraded_on_invalid: false,
            location: std::panic::Location::caller(),
        };

        let mut ctx = PendingInvalidCtx;
        let poll = AnchorInner::poll_updated(&mut either, &mut ctx);
        assert_eq!(Poll::PendingInvalidToken, poll);
        assert!(!either.degraded_on_invalid);
    }
}
