use crate::expert::{
    Anchor, AnchorHandle, AnchorInner, Engine, OutputContext, Poll, UpdateContext,
};
use std::panic::Location;

pub struct Then<A, Out, F, E: Engine> {
    pub(super) f: F,
    pub(super) f_anchor: Option<Anchor<Out, E>>,
    pub(super) output_stale: bool,
    pub(super) anchors: A,
    pub(super) location: &'static Location<'static>,
}

/// `then` 的“owned 输出缓存”版本（单入参）。
///
/// 设计目标：在 slotmap 模式的拆树/GC 期，避免 `output()` 再去 `ctx.get(f_anchor)` 触发硬崩溃。
///
/// - 普通 `then`：output 直接返回 `ctx.get(f_anchor)`（借用输出），当 f_anchor token 失效会 panic。
/// - `ThenClone1`：在 `poll_updated` 阶段 clone 输出并缓存，`output()` 仅返回缓存引用。
///   - 若遇到 `PendingInvalidToken`：冻结并保留缓存（返回 `Unchanged`），避免重复 request 与崩溃。
pub struct ThenClone1<O1, Out, F, E: Engine> {
    pub(super) f: F,
    pub(super) f_anchor: Option<Anchor<Out, E>>,
    pub(super) cached_output: Out,
    pub(super) degraded_on_invalid: bool,
    pub(super) output_stale: bool,
    pub(super) output_dirty: bool,
    pub(super) anchor: Anchor<O1, E>,
    pub(super) location: &'static Location<'static>,
}

macro_rules! impl_tuple_then {
    ($([$output_type:ident, $num:tt])+) => {
        impl<$($output_type,)+ E, F, Out> AnchorInner<E> for
            Then<( $(Anchor<$output_type, E>,)+ ), Out, F, E>
        where
            F: for<'any> FnMut($(&'any $output_type),+) -> Anchor<Out, E>,
            Out: 'static,
            $(
                $output_type: 'static,
            )+
            E: Engine,
        {
            type Output = Out;
            fn dirty(&mut self, edge: &<E::AnchorHandle as AnchorHandle>::Token) {
                if self.output_stale {
                    return;
                }

                $(
                    // only invalidate f_anchor if one of the lhs anchors is invalidated
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
                if self.f_anchor.is_none() || self.output_stale {
                    if emg_debug_env::bool_lenient("ANCHORS_DEBUG_THEN") {
                        println!(
                            "THEN poll: output_stale={} f_anchor_present={} loc={:?}",
                            self.output_stale,
                            self.f_anchor.is_some(),
                            self.location
                        );
                    }
                    let mut found_pending = false;
                    let mut found_invalid = false;

                    $(
                        let poll = ctx.request(&self.anchors.$num, true);
                        if poll.is_waiting() {
                            found_pending = true;
                            self.output_stale = true;
                        } else if poll.is_invalid_token() {
                                ////////////////////////////////////////////////////////////////////////////////
                                // NOTE:
                                // - slotmap 模式下，拆树/GC 期间可能遇到 “依赖 token 已失效”。
                                // - then 的输出是“另一个 Anchor 的输出”，如果已经有旧的 f_anchor，
                                //   我们可以直接复用旧输出，避免把 Pending 继续向上传播导致 stabilize 自旋。
                                ////////////////////////////////////////////////////////////////////////////////
                                found_invalid = true;
                        } else if poll == Poll::Updated {
                            self.output_stale = true;
                        }
                    )+

                    if found_invalid {
                        // 已经有历史输出：直接降级复用旧输出，避免重复请求失效 token。
                        if let Some(old_anchor) = self.f_anchor.as_ref() {
                            self.output_stale = false;
                            return ctx.request(old_anchor, true);
                        }

                        self.output_stale = true;
                        return Poll::PendingInvalidToken;
                    }

                    if found_pending {
                        #[cfg(debug_assertions)]
                        {
                            if emg_debug_env::bool_lenient("ANCHORS_DEBUG_THEN") {
                                tracing::warn!(
                                    target: "anchors",
                                    "then pending: loc={:?} anchors={}",
                                    self.location,
                                    stringify!($($output_type),+)
                                );
                            }
                        }
                        return Poll::Pending;
                    }

                    self.output_stale = false;
                    let new_anchor = (self.f)($(ctx.get(&self.anchors.$num)),+);
                    match self.f_anchor.as_ref() {
                        Some(outdated_anchor) if outdated_anchor != &new_anchor => {
                            #[cfg(debug_assertions)]
                            if emg_debug_env::bool_lenient("ANCHORS_DEBUG_THEN") {
                                tracing::warn!(
                                    target: "anchors",
                                    "then switching output anchor old={:?} new={:?} loc={:?}",
                                    outdated_anchor.token(),
                                    new_anchor.token(),
                                    self.location
                                );
                            }
                            if emg_debug_env::bool_lenient("ANCHORS_DEBUG_THEN") {
                                println!(
                                    "THEN 切换输出 Anchor old={:?} new={:?} loc={:?}",
                                    outdated_anchor.token(),
                                    new_anchor.token(),
                                    self.location
                                );
                            }
                            ctx.unrequest(outdated_anchor);
                            self.f_anchor = Some(new_anchor);
                        }
                        None => {
                            self.f_anchor = Some(new_anchor);
                        }
                        _ => {
                            // 相同输出 Anchor，复用即可
                        }
                    }
                }

                ctx.request(self.f_anchor.as_ref().unwrap(), true)
            }
            fn output<'slf, 'out, G: OutputContext<'out, Engine=E>>(
                &'slf self,
                ctx: &mut G,
            ) -> &'out Self::Output
            where
                'slf: 'out,
            {
                ctx.get(self.f_anchor.as_ref().unwrap())
            }

            fn debug_location(&self) -> Option<(&'static str, &'static Location<'static>)> {
                Some(("then", self.location))
            }
        }
    }
}

/// 单入参去重版本：仅在输入值未变时复用上一轮输出 Anchor，减少 token 切换带来的高度回填。
pub struct ThenDedupe1<O1, Out, F, E: Engine> {
    pub(super) f: F,
    pub(super) f_anchor: Option<Anchor<Out, E>>,
    pub(super) cached_input: Option<O1>,
    pub(super) output_stale: bool,
    pub(super) anchor: Anchor<O1, E>,
    pub(super) location: &'static Location<'static>,
}

/// 多入参去重版本：输入元组实现 PartialEq 时可复用旧输出 Anchor。
pub struct ThenDedupe<A, Out, F, E: Engine> {
    pub(super) f: F,
    pub(super) f_anchor: Option<Anchor<Out, E>>,
    pub(super) output_stale: bool,
    pub(super) anchors: A,
    pub(super) location: &'static Location<'static>,
    pub(super) cached_inputs: Option<Box<dyn std::any::Any>>,
}

impl<O1, E, F, Out> AnchorInner<E> for ThenClone1<O1, Out, F, E>
where
    F: for<'a> FnMut(&'a O1) -> Anchor<Out, E>,
    Out: 'static + Clone,
    O1: 'static,
    E: Engine,
{
    type Output = Out;

    fn dirty(&mut self, edge: &<E::AnchorHandle as AnchorHandle>::Token) {
        let debug_then = emg_debug_env::bool_lenient("ANCHORS_DEBUG_THEN")
            && self.location.file().ends_with("node_item_rc_sv.rs");
        if edge == &self.anchor.data.token() {
            self.degraded_on_invalid = false;
            self.output_stale = true;
            if debug_then {
                let input_token = self.anchor.token();
                let output_token = self.f_anchor.as_ref().map(|anchor| anchor.token());
                println!(
                    "THEN_CLONE1 dirty input token={input_token:?} output_token={output_token:?} degraded={} output_stale={} output_dirty={} loc={:?}",
                    self.degraded_on_invalid, self.output_stale, self.output_dirty, self.location
                );
            }
            return;
        }

        if let Some(out_anchor) = self.f_anchor.as_ref() {
            if edge == &out_anchor.data.token() {
                self.output_dirty = true;
                if debug_then {
                    let input_token = self.anchor.token();
                    let output_token = out_anchor.token();
                    println!(
                        "THEN_CLONE1 dirty output token={output_token:?} input_token={input_token:?} degraded={} output_stale={} output_dirty={} loc={:?}",
                        self.degraded_on_invalid,
                        self.output_stale,
                        self.output_dirty,
                        self.location
                    );
                }
                return;
            }
        }

        if self.output_stale {
            return;
        }
    }

    fn poll_updated<G: UpdateContext<Engine = E>>(&mut self, ctx: &mut G) -> Poll {
        let debug_then = emg_debug_env::bool_lenient("ANCHORS_DEBUG_THEN")
            && self.location.file().ends_with("node_item_rc_sv.rs");
        if debug_then {
            let input_token = self.anchor.token();
            let output_token = self.f_anchor.as_ref().map(|anchor| anchor.token());
            println!(
                "THEN_CLONE1 poll start degraded={} output_dirty={} output_stale={} input_token={input_token:?} output_token={output_token:?} loc={:?}",
                self.degraded_on_invalid, self.output_dirty, self.output_stale, self.location
            );
        }
        // 降级冻结模式：
        // - 仍然需要 request 输入，用于重建 clean_parent 链路（上游 Updated 会 drain 父链路）。
        // - 只有当输入 Updated 时，才尝试解除冻结并重新选择输出 Anchor。
        if self.degraded_on_invalid {
            let poll = ctx.request(&self.anchor, true);
            if poll.is_waiting() || poll.is_invalid_token() {
                if debug_then {
                    let input_token = self.anchor.token();
                    let output_token = self.f_anchor.as_ref().map(|anchor| anchor.token());
                    println!(
                        "THEN_CLONE1 degraded input poll={poll:?} input_token={input_token:?} output_token={output_token:?} loc={:?}",
                        self.location
                    );
                }
                return Poll::Unchanged;
            }

            if self.output_dirty {
                if let Some(out_anchor) = self.f_anchor.as_ref() {
                    let out_poll = ctx.request(out_anchor, true);
                    if debug_then {
                        let input_token = self.anchor.token();
                        let output_token = out_anchor.token();
                        println!(
                            "THEN_CLONE1 degraded output poll={out_poll:?} input_poll={poll:?} input_token={input_token:?} output_token={output_token:?} loc={:?}",
                            self.location
                        );
                    }
                    match out_poll {
                        Poll::PendingInvalidToken => {
                            self.output_dirty = false;
                            self.output_stale = true;
                            return Poll::Unchanged;
                        }
                        Poll::Updated => {
                            self.cached_output = ctx.get(out_anchor).clone();
                            self.output_dirty = false;
                            self.degraded_on_invalid = false;
                            self.output_stale = false;
                            return Poll::Updated;
                        }
                        Poll::Unchanged => {
                            self.output_dirty = false;
                            self.degraded_on_invalid = false;
                            self.output_stale = false;
                            return Poll::Unchanged;
                        }
                        Poll::Pending => return Poll::Pending,
                        #[cfg(feature = "anchors_pending_queue")]
                        Poll::PendingDefer => return Poll::Pending,
                    }
                }
            }

            if poll != Poll::Updated && !self.output_stale {
                if debug_then {
                    let input_token = self.anchor.token();
                    let output_token = self.f_anchor.as_ref().map(|anchor| anchor.token());
                    println!(
                        "THEN_CLONE1 degraded keep old output poll={poll:?} input_token={input_token:?} output_token={output_token:?} output_stale={} loc={:?}",
                        self.output_stale, self.location
                    );
                }
                return Poll::Unchanged;
            }
            self.degraded_on_invalid = false;
            self.output_stale = true;
        }

        let mut switched_output_anchor = false;

        if self.f_anchor.is_none() || self.output_stale {
            let poll = ctx.request(&self.anchor, true);
            if poll.is_waiting() {
                self.output_stale = true;
                return Poll::Pending;
            }
            if poll.is_invalid_token() {
                self.degraded_on_invalid = true;
                self.output_stale = false;
                self.output_dirty = false;
                if debug_then {
                    let input_token = self.anchor.token();
                    let output_token = self.f_anchor.as_ref().map(|anchor| anchor.token());
                    println!(
                        "THEN_CLONE1 enter degraded input_poll={poll:?} input_token={input_token:?} output_token={output_token:?} loc={:?}",
                        self.location
                    );
                }
                return Poll::Unchanged;
            }
            if poll == Poll::Updated {
                self.output_stale = true;
            }

            self.output_stale = false;
            let new_anchor = (self.f)(ctx.get(&self.anchor));
            match self.f_anchor.as_ref() {
                Some(old_anchor) if old_anchor != &new_anchor => {
                    ctx.unrequest(old_anchor);
                    self.f_anchor = Some(new_anchor);
                    switched_output_anchor = true;
                }
                None => {
                    self.f_anchor = Some(new_anchor);
                    switched_output_anchor = true;
                }
                _ => {
                    // 输出 anchor 未变化，复用即可
                }
            }
            self.output_dirty = false;
        }

        let out_anchor = self
            .f_anchor
            .as_ref()
            .expect("then_clone1: f_anchor must exist");
        let out_poll = ctx.request(out_anchor, true);
        if out_poll.is_waiting() {
            return Poll::Pending;
        }
        match out_poll {
            Poll::PendingInvalidToken => {
                self.degraded_on_invalid = true;
                self.output_dirty = false;
                self.output_stale = true;
                if debug_then {
                    let input_token = self.anchor.token();
                    let output_token = out_anchor.token();
                    println!(
                        "THEN_CLONE1 output invalid token input_token={input_token:?} output_token={output_token:?} loc={:?}",
                        self.location
                    );
                }
                let _ = ctx.request(&self.anchor, true);
                Poll::Unchanged
            }
            Poll::Updated => {
                self.cached_output = ctx.get(out_anchor).clone();
                self.output_dirty = false;
                Poll::Updated
            }
            Poll::Unchanged => {
                if switched_output_anchor || self.output_dirty {
                    self.cached_output = ctx.get(out_anchor).clone();
                    self.output_dirty = false;
                    return Poll::Updated;
                }
                Poll::Unchanged
            }
            Poll::Pending => Poll::Pending,
            #[cfg(feature = "anchors_pending_queue")]
            Poll::PendingDefer => Poll::Pending,
        }
    }

    fn output<'slf, 'out, G: OutputContext<'out, Engine = E>>(
        &'slf self,
        _ctx: &mut G,
    ) -> &'out Self::Output
    where
        'slf: 'out,
    {
        &self.cached_output
    }

    fn debug_location(&self) -> Option<(&'static str, &'static Location<'static>)> {
        Some(("then_clone1", self.location))
    }
}

impl<O1, E, F, Out> AnchorInner<E> for ThenDedupe1<O1, Out, F, E>
where
    F: for<'a> FnMut(&'a O1) -> Anchor<Out, E>,
    Out: 'static,
    O1: 'static + Clone + PartialEq,
    E: Engine,
{
    type Output = Out;

    fn dirty(&mut self, edge: &<E::AnchorHandle as AnchorHandle>::Token) {
        if self.output_stale {
            return;
        }
        if edge == &self.anchor.data.token() {
            self.output_stale = true;
        }
    }

    fn poll_updated<G: UpdateContext<Engine = E>>(&mut self, ctx: &mut G) -> Poll {
        if self.f_anchor.is_none() || self.output_stale {
            let poll = ctx.request(&self.anchor, true);
            if poll.is_waiting() {
                self.output_stale = true;
                return Poll::Pending;
            }
            if poll.is_invalid_token() {
                // 有历史输出时可直接复用旧输出，避免 then_dedupe1 反复 pending。
                if let Some(old_anchor) = self.f_anchor.as_ref() {
                    self.output_stale = false;
                    return ctx.request(old_anchor, true);
                }
                self.output_stale = true;
                return Poll::PendingInvalidToken;
            }
            if poll == Poll::Updated {
                self.output_stale = true;
            }

            let cur = ctx.get(&self.anchor).clone();
            if let (Some(old_anchor), Some(prev)) = (&self.f_anchor, &self.cached_input) {
                if prev == &cur {
                    self.output_stale = false;
                    return ctx.request(old_anchor, true);
                }
            }

            self.output_stale = false;
            let new_anchor = (self.f)(ctx.get(&self.anchor));
            if let Some(old) = self.f_anchor.replace(new_anchor) {
                ctx.unrequest(&old);
            }
            self.cached_input = Some(cur);
        }
        ctx.request(self.f_anchor.as_ref().unwrap(), true)
    }

    fn output<'slf, 'out, G: OutputContext<'out, Engine = E>>(
        &'slf self,
        ctx: &mut G,
    ) -> &'out Self::Output
    where
        'slf: 'out,
    {
        ctx.get(self.f_anchor.as_ref().unwrap())
    }

    fn debug_location(&self) -> Option<(&'static str, &'static Location<'static>)> {
        Some(("then_dedupe1", self.location))
    }
}

/// 生成多入参去重版本（2~3 元），要求输入 Clone + PartialEq。
macro_rules! impl_tuple_then_dedupe {
    ($([$output_type:ident, $num:tt])+) => {
        impl<$($output_type,)+ E, F, Out> AnchorInner<E> for
            ThenDedupe<( $(Anchor<$output_type, E>,)+ ), Out, F, E>
        where
            F: for<'any> FnMut($(&'any $output_type),+) -> Anchor<Out, E>,
            Out: 'static,
            $(
                $output_type: 'static + Clone + PartialEq,
            )+
            E: Engine,
        {
            type Output = Out;
            fn dirty(&mut self, edge: &<E::AnchorHandle as AnchorHandle>::Token) {
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
                if self.f_anchor.is_none() || self.output_stale {
                    let mut found_pending = false;
                    let mut found_invalid = false;
                    $(
                        let poll = ctx.request(&self.anchors.$num, true);
                        if poll.is_waiting() {
                            found_pending = true;
                            self.output_stale = true;
                        } else if poll.is_invalid_token() {
                            // 有历史输出时可降级复用旧输出，避免稳定化自旋。
                            found_invalid = true;
                        } else if poll == Poll::Updated {
                            self.output_stale = true;
                        }
                    )+

                    if found_invalid {
                        if let Some(old_anchor) = self.f_anchor.as_ref() {
                            self.output_stale = false;
                            return ctx.request(old_anchor, true);
                        }
                        self.output_stale = true;
                        return Poll::PendingInvalidToken;
                    }
                    if found_pending {
                        return Poll::Pending;
                    }

                    let current_inputs = ($(ctx.get(&self.anchors.$num).clone(),)+);
                    if let (Some(old_anchor), Some(prev_any)) =
                        (&self.f_anchor, &self.cached_inputs)
                    {
                        if let Some(prev) = prev_any.downcast_ref::<($( $output_type ,)+)>() {
                            if prev == &current_inputs {
                                self.output_stale = false;
                                return ctx.request(old_anchor, true);
                            }
                        }
                    }

                    self.output_stale = false;
                    let new_anchor = (self.f)($(ctx.get(&self.anchors.$num)),+);
                    if let Some(old) = self.f_anchor.replace(new_anchor) {
                        ctx.unrequest(&old);
                    }
                    self.cached_inputs = Some(Box::new(current_inputs));
                }
                ctx.request(self.f_anchor.as_ref().unwrap(), true)
            }

            fn output<'slf, 'out, G: OutputContext<'out, Engine=E>>(
                &'slf self,
                ctx: &mut G,
            ) -> &'out Self::Output
            where
                'slf: 'out,
            {
                ctx.get(self.f_anchor.as_ref().unwrap())
            }

            fn debug_location(&self) -> Option<(&'static str, &'static Location<'static>)> {
                Some(("then_dedupe", self.location))
            }
        }
    }
}

impl_tuple_then! {
    [O0, 0]
}

impl_tuple_then! {
    [O0, 0]
    [O1, 1]
}
impl_tuple_then! {
    [O0, 0]
    [O1, 1]
    [O2, 2]
}

impl_tuple_then! {
    [O0, 0]
    [O1, 1]
    [O2, 2]
    [O3, 3]
}

impl_tuple_then! {
    [O0, 0]
    [O1, 1]
    [O2, 2]
    [O3, 3]
    [O4, 4]
}

impl_tuple_then! {
    [O0, 0]
    [O1, 1]
    [O2, 2]
    [O3, 3]
    [O4, 4]
    [O5, 5]
}

impl_tuple_then! {
    [O0, 0]
    [O1, 1]
    [O2, 2]
    [O3, 3]
    [O4, 4]
    [O5, 5]
    [O6, 6]
}

impl_tuple_then! {
    [O0, 0]
    [O1, 1]
    [O2, 2]
    [O3, 3]
    [O4, 4]
    [O5, 5]
    [O6, 6]
    [O7, 7]
}

impl_tuple_then! {
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
    use crate::singlethread::Engine;
    use std::cell::Cell;
    use std::rc::Rc;

    type Token =
        <<Engine as crate::expert::Engine>::AnchorHandle as crate::expert::AnchorHandle>::Token;

    /// ////////////////////////////////////////////////////////////////////////////
    /// 单测：then 在遇到 PendingInvalidToken 时，如果已有历史输出，应直接复用旧输出，
    /// 避免把 Pending 继续向上传播导致 stabilize 自旋/重复 invalid_token 日志。
    /// ////////////////////////////////////////////////////////////////////////////
    #[test]
    fn then_should_degrade_on_pending_invalid_token_when_has_output() {
        let _engine = Engine::new();

        let input: Anchor<u32, Engine> = Anchor::constant(1);
        let output: Anchor<u32, Engine> = Anchor::constant(2);

        struct Ctx {
            input_token: Token,
            output_token: Token,
            requested_input: Cell<u32>,
            requested_output: Cell<u32>,
        }

        impl crate::expert::UpdateContext for Ctx {
            type Engine = Engine;

            fn get<'out, 'slf, O: 'static>(&'slf self, _anchor: &Anchor<O, Self::Engine>) -> &'out O
            where
                'slf: 'out,
            {
                panic!("then 在 PendingInvalidToken 降级分支不应调用 get()");
            }

            fn request<O: 'static>(
                &mut self,
                anchor: &Anchor<O, Self::Engine>,
                _necessary: bool,
            ) -> Poll {
                let token = anchor.token();
                if token == self.input_token {
                    self.requested_input.set(self.requested_input.get() + 1);
                    return Poll::PendingInvalidToken;
                }
                if token == self.output_token {
                    self.requested_output.set(self.requested_output.get() + 1);
                    return Poll::Unchanged;
                }
                panic!("unexpected token requested: {token:?}");
            }

            fn unrequest<O: 'static>(&mut self, _anchor: &Anchor<O, Self::Engine>) {
                panic!("then 降级分支不应调用 unrequest()");
            }

            fn dirty_handle(&mut self) -> <Self::Engine as crate::expert::Engine>::DirtyHandle {
                panic!("then 降级分支不应申请 dirty_handle()");
            }
        }

        let called = Rc::new(Cell::new(false));
        let called2 = called.clone();
        let output_for_f = output.clone();

        let mut inner = Then::<(Anchor<u32, Engine>,), u32, _, Engine> {
            f: move |_v: &u32| {
                called2.set(true);
                output_for_f.clone()
            },
            f_anchor: Some(output.clone()),
            output_stale: true,
            anchors: (input.clone(),),
            location: Location::caller(),
        };

        let mut ctx = Ctx {
            input_token: input.token(),
            output_token: output.token(),
            requested_input: Cell::new(0),
            requested_output: Cell::new(0),
        };

        let poll = inner.poll_updated(&mut ctx);
        assert_eq!(poll, Poll::Unchanged);
        assert!(!called.get(), "then 不应在 invalid token 下重新计算 f()");
        assert_eq!(ctx.requested_input.get(), 1);
        assert_eq!(ctx.requested_output.get(), 1);
    }

    /// ////////////////////////////////////////////////////////////////////////////
    /// 单测：then_dedupe1 在 PendingInvalidToken 时，如果已有历史输出，应复用旧输出。
    /// ////////////////////////////////////////////////////////////////////////////
    #[test]
    fn then_dedupe1_should_degrade_on_pending_invalid_token_when_has_output() {
        let _engine = Engine::new();

        let input: Anchor<u32, Engine> = Anchor::constant(1);
        let output: Anchor<u32, Engine> = Anchor::constant(2);

        struct Ctx {
            input_token: Token,
            output_token: Token,
            requested_input: Cell<u32>,
            requested_output: Cell<u32>,
        }

        impl crate::expert::UpdateContext for Ctx {
            type Engine = Engine;

            fn get<'out, 'slf, O: 'static>(&'slf self, _anchor: &Anchor<O, Self::Engine>) -> &'out O
            where
                'slf: 'out,
            {
                panic!("then_dedupe1 在 PendingInvalidToken 降级分支不应调用 get()");
            }

            fn request<O: 'static>(
                &mut self,
                anchor: &Anchor<O, Self::Engine>,
                _necessary: bool,
            ) -> Poll {
                let token = anchor.token();
                if token == self.input_token {
                    self.requested_input.set(self.requested_input.get() + 1);
                    return Poll::PendingInvalidToken;
                }
                if token == self.output_token {
                    self.requested_output.set(self.requested_output.get() + 1);
                    return Poll::Unchanged;
                }
                panic!("unexpected token requested: {token:?}");
            }

            fn unrequest<O: 'static>(&mut self, _anchor: &Anchor<O, Self::Engine>) {
                panic!("then_dedupe1 降级分支不应调用 unrequest()");
            }

            fn dirty_handle(&mut self) -> <Self::Engine as crate::expert::Engine>::DirtyHandle {
                panic!("then_dedupe1 降级分支不应申请 dirty_handle()");
            }
        }

        let called = Rc::new(Cell::new(false));
        let called2 = called.clone();
        let output_for_f = output.clone();

        let mut inner = ThenDedupe1::<u32, u32, _, Engine> {
            f: move |_v: &u32| {
                called2.set(true);
                output_for_f.clone()
            },
            f_anchor: Some(output.clone()),
            cached_input: Some(123),
            output_stale: true,
            anchor: input.clone(),
            location: Location::caller(),
        };

        let mut ctx = Ctx {
            input_token: input.token(),
            output_token: output.token(),
            requested_input: Cell::new(0),
            requested_output: Cell::new(0),
        };

        let poll = inner.poll_updated(&mut ctx);
        assert_eq!(poll, Poll::Unchanged);
        assert!(
            !called.get(),
            "then_dedupe1 不应在 invalid token 下重新计算 f()"
        );
        assert_eq!(ctx.requested_input.get(), 1);
        assert_eq!(ctx.requested_output.get(), 1);
    }

    /// ////////////////////////////////////////////////////////////////////////////
    /// 单测：then_clone1 在 output() 阶段不应调用 ctx.get(f_anchor)。
    /// ////////////////////////////////////////////////////////////////////////////
    #[test]
    fn then_clone1_output_should_not_call_ctx_get() {
        let _engine = Engine::new();

        let input: Anchor<u32, Engine> = Anchor::constant(1);
        let output: Anchor<u32, Engine> = Anchor::constant(2);
        let output_for_f = output.clone();

        struct Ctx;

        impl<'eng> crate::expert::OutputContext<'eng> for Ctx {
            type Engine = Engine;

            fn get<'out, O: 'static>(&self, _anchor: &Anchor<O, Self::Engine>) -> &'out O
            where
                'eng: 'out,
            {
                panic!("then_clone1::output 不应调用 ctx.get()");
            }
        }

        let inner = ThenClone1::<u32, u32, _, Engine> {
            f: move |_v: &u32| output_for_f.clone(),
            f_anchor: Some(output),
            cached_output: 777,
            degraded_on_invalid: false,
            output_stale: false,
            output_dirty: false,
            anchor: input,
            location: Location::caller(),
        };

        let mut ctx = Ctx;
        let out = inner.output(&mut ctx);
        assert_eq!(*out, 777);
    }

    /// ////////////////////////////////////////////////////////////////////////////
    /// 单测：then_clone1 在输入 PendingInvalidToken 时应冻结并保留 cached_output。
    /// ////////////////////////////////////////////////////////////////////////////
    #[test]
    fn then_clone1_should_freeze_on_pending_invalid_token_input() {
        let _engine = Engine::new();

        let input: Anchor<u32, Engine> = Anchor::constant(1);
        let output: Anchor<u32, Engine> = Anchor::constant(2);

        struct Ctx {
            input_token: Token,
            output_token: Token,
            requested_input: Cell<u32>,
            requested_output: Cell<u32>,
        }

        impl crate::expert::UpdateContext for Ctx {
            type Engine = Engine;

            fn get<'out, 'slf, O: 'static>(&'slf self, _anchor: &Anchor<O, Self::Engine>) -> &'out O
            where
                'slf: 'out,
            {
                panic!("then_clone1 冻结分支不应调用 get()");
            }

            fn request<O: 'static>(
                &mut self,
                anchor: &Anchor<O, Self::Engine>,
                _necessary: bool,
            ) -> Poll {
                let token = anchor.token();
                if token == self.input_token {
                    self.requested_input.set(self.requested_input.get() + 1);
                    return Poll::PendingInvalidToken;
                }
                if token == self.output_token {
                    self.requested_output.set(self.requested_output.get() + 1);
                    return Poll::Unchanged;
                }
                panic!("unexpected token requested: {token:?}");
            }

            fn unrequest<O: 'static>(&mut self, _anchor: &Anchor<O, Self::Engine>) {
                panic!("then_clone1 冻结分支不应调用 unrequest()");
            }

            fn dirty_handle(&mut self) -> <Self::Engine as crate::expert::Engine>::DirtyHandle {
                panic!("then_clone1 冻结分支不应申请 dirty_handle()");
            }
        }

        let called = Rc::new(Cell::new(false));
        let called2 = called.clone();
        let output_for_f = output.clone();

        let mut inner = ThenClone1::<u32, u32, _, Engine> {
            f: move |_v: &u32| {
                called2.set(true);
                output_for_f.clone()
            },
            f_anchor: Some(output),
            cached_output: 42,
            degraded_on_invalid: false,
            output_stale: true,
            output_dirty: false,
            anchor: input,
            location: Location::caller(),
        };

        let mut ctx = Ctx {
            input_token: inner.anchor.token(),
            output_token: inner.f_anchor.as_ref().unwrap().token(),
            requested_input: Cell::new(0),
            requested_output: Cell::new(0),
        };

        let poll = inner.poll_updated(&mut ctx);
        assert_eq!(poll, Poll::Unchanged);
        assert!(
            !called.get(),
            "then_clone1 不应在 invalid token 下重新计算 f()"
        );
        assert_eq!(ctx.requested_input.get(), 1);
        assert_eq!(ctx.requested_output.get(), 0);
        assert!(inner.degraded_on_invalid, "then_clone1 应进入冻结模式");
    }

    #[test]
    fn then_clone1_degraded_should_request_input() {
        let _engine = Engine::new();

        let input: Anchor<u32, Engine> = Anchor::constant(1);
        let output: Anchor<u32, Engine> = Anchor::constant(2);

        struct Ctx {
            input_token: Token,
            output_token: Token,
            requested_input: Cell<u32>,
        }

        impl crate::expert::UpdateContext for Ctx {
            type Engine = Engine;

            fn get<'out, 'slf, O: 'static>(&'slf self, _anchor: &Anchor<O, Self::Engine>) -> &'out O
            where
                'slf: 'out,
            {
                panic!("then_clone1 冻结模式不应调用 get()")
            }

            fn request<O: 'static>(
                &mut self,
                anchor: &Anchor<O, Self::Engine>,
                _necessary: bool,
            ) -> Poll {
                let token = anchor.token();
                if token == self.input_token {
                    self.requested_input.set(self.requested_input.get() + 1);
                    return Poll::Unchanged;
                }
                if token == self.output_token {
                    panic!("then_clone1 冻结模式不应 request 输出 anchor")
                }
                panic!("unexpected token requested: {token:?}");
            }

            fn unrequest<O: 'static>(&mut self, _anchor: &Anchor<O, Self::Engine>) {
                panic!("then_clone1 冻结模式不应调用 unrequest()")
            }

            fn dirty_handle(&mut self) -> <Self::Engine as crate::expert::Engine>::DirtyHandle {
                panic!("then_clone1 冻结模式不应申请 dirty_handle()")
            }
        }

        let mut inner = ThenClone1::<u32, u32, _, Engine> {
            f: |_v: &u32| panic!("then_clone1 冻结模式不应重新计算 f()"),
            f_anchor: Some(output.clone()),
            cached_output: 42,
            degraded_on_invalid: true,
            output_stale: false,
            output_dirty: false,
            anchor: input.clone(),
            location: Location::caller(),
        };

        let mut ctx = Ctx {
            input_token: input.token(),
            output_token: output.token(),
            requested_input: Cell::new(0),
        };

        let poll = inner.poll_updated(&mut ctx);
        assert_eq!(poll, Poll::Unchanged);
        assert_eq!(ctx.requested_input.get(), 1);
        assert!(inner.degraded_on_invalid);
    }

    #[test]
    fn then_clone1_degraded_should_recover_on_output_dirty() {
        let _engine = Engine::new();

        let input: Anchor<u32, Engine> = Anchor::constant(1);
        let output: Anchor<u32, Engine> = Anchor::constant(2);

        struct Ctx {
            input_token: Token,
            output_token: Token,
            input_value: u32,
            output_value: u32,
            requested_input: Cell<u32>,
            requested_output: Cell<u32>,
        }

        impl crate::expert::UpdateContext for Ctx {
            type Engine = Engine;

            fn get<'out, 'slf, O: 'static>(&'slf self, anchor: &Anchor<O, Self::Engine>) -> &'out O
            where
                'slf: 'out,
            {
                use std::any::Any;

                let token = anchor.token();
                if token == self.input_token {
                    return (&self.input_value as &dyn Any)
                        .downcast_ref::<O>()
                        .expect("unexpected input type");
                }
                if token == self.output_token {
                    return (&self.output_value as &dyn Any)
                        .downcast_ref::<O>()
                        .expect("unexpected output type");
                }
                panic!("unexpected token requested: {token:?}");
            }

            fn request<O: 'static>(
                &mut self,
                anchor: &Anchor<O, Self::Engine>,
                _necessary: bool,
            ) -> Poll {
                let token = anchor.token();
                if token == self.input_token {
                    self.requested_input
                        .set(self.requested_input.get().saturating_add(1));
                    return Poll::Unchanged;
                }
                if token == self.output_token {
                    self.requested_output
                        .set(self.requested_output.get().saturating_add(1));
                    return Poll::Updated;
                }
                panic!("unexpected token requested: {token:?}");
            }

            fn unrequest<O: 'static>(&mut self, _anchor: &Anchor<O, Self::Engine>) {
                panic!("then_clone1 恢复测试不应调用 unrequest()");
            }

            fn dirty_handle(&mut self) -> <Self::Engine as crate::expert::Engine>::DirtyHandle {
                panic!("then_clone1 恢复测试不应申请 dirty_handle()");
            }
        }

        let output_for_f = output.clone();
        let mut inner = ThenClone1::<u32, u32, _, Engine> {
            f: move |_v: &u32| output_for_f.clone(),
            f_anchor: Some(output.clone()),
            cached_output: 42,
            degraded_on_invalid: true,
            output_stale: false,
            output_dirty: false,
            anchor: input.clone(),
            location: Location::caller(),
        };

        inner.dirty(&output.token());

        let mut ctx = Ctx {
            input_token: input.token(),
            output_token: output.token(),
            input_value: 1,
            output_value: 99,
            requested_input: Cell::new(0),
            requested_output: Cell::new(0),
        };

        let poll = inner.poll_updated(&mut ctx);
        assert_eq!(poll, Poll::Updated);
        assert_eq!(ctx.requested_input.get(), 1);
        assert_eq!(ctx.requested_output.get(), 1);
        assert!(!inner.degraded_on_invalid, "应当从冻结模式恢复");
        assert!(!inner.output_dirty);
        assert_eq!(inner.cached_output, 99);
    }

    #[test]
    fn then_clone1_should_recover_after_output_invalid_when_input_ready() {
        let _engine = Engine::new();

        let input: Anchor<u32, Engine> = Anchor::constant(1);
        let output_old: Anchor<u32, Engine> = Anchor::constant(2);
        let output_new: Anchor<u32, Engine> = Anchor::constant(3);

        struct Ctx {
            phase: u8,
            input_token: Token,
            output_old_token: Token,
            output_new_token: Token,
            input_value: u32,
            output_new_value: u32,
            requested_input: Cell<u32>,
            requested_output_old: Cell<u32>,
            requested_output_new: Cell<u32>,
            unrequested_output_old: Cell<u32>,
        }

        impl crate::expert::UpdateContext for Ctx {
            type Engine = Engine;

            fn get<'out, 'slf, O: 'static>(&'slf self, anchor: &Anchor<O, Self::Engine>) -> &'out O
            where
                'slf: 'out,
            {
                use std::any::Any;

                let token = anchor.token();
                if token == self.input_token {
                    return (&self.input_value as &dyn Any)
                        .downcast_ref::<O>()
                        .expect("unexpected input type");
                }
                if token == self.output_new_token {
                    return (&self.output_new_value as &dyn Any)
                        .downcast_ref::<O>()
                        .expect("unexpected output type");
                }
                if token == self.output_old_token {
                    panic!("old output should not be read after invalid token");
                }
                panic!("unexpected token requested: {token:?}");
            }

            fn request<O: 'static>(
                &mut self,
                anchor: &Anchor<O, Self::Engine>,
                _necessary: bool,
            ) -> Poll {
                let token = anchor.token();
                if token == self.input_token {
                    self.requested_input.set(self.requested_input.get() + 1);
                    return Poll::Unchanged;
                }
                if token == self.output_old_token {
                    self.requested_output_old
                        .set(self.requested_output_old.get() + 1);
                    return Poll::PendingInvalidToken;
                }
                if token == self.output_new_token {
                    self.requested_output_new
                        .set(self.requested_output_new.get() + 1);
                    return match self.phase {
                        0 => Poll::PendingInvalidToken,
                        1 => Poll::Updated,
                        _ => panic!("unexpected phase={}", self.phase),
                    };
                }
                panic!("unexpected token requested: {token:?}");
            }

            fn unrequest<O: 'static>(&mut self, anchor: &Anchor<O, Self::Engine>) {
                let token = anchor.token();
                if token == self.output_old_token {
                    self.unrequested_output_old
                        .set(self.unrequested_output_old.get() + 1);
                    return;
                }
                panic!("unexpected token unrequested: {token:?}");
            }

            fn dirty_handle(&mut self) -> <Self::Engine as crate::expert::Engine>::DirtyHandle {
                panic!("then_clone1 恢复路径不应申请 dirty_handle()");
            }
        }

        let called = Rc::new(Cell::new(0));
        let called2 = called.clone();
        let output_for_f = output_new.clone();

        let mut inner = ThenClone1::<u32, u32, _, Engine> {
            f: move |_v: &u32| {
                called2.set(called2.get() + 1);
                output_for_f.clone()
            },
            f_anchor: Some(output_old),
            cached_output: 42,
            degraded_on_invalid: false,
            output_stale: false,
            output_dirty: false,
            anchor: input,
            location: Location::caller(),
        };

        let mut ctx = Ctx {
            phase: 0,
            input_token: inner.anchor.token(),
            output_old_token: inner.f_anchor.as_ref().unwrap().token(),
            output_new_token: output_new.token(),
            input_value: 1,
            output_new_value: 777,
            requested_input: Cell::new(0),
            requested_output_old: Cell::new(0),
            requested_output_new: Cell::new(0),
            unrequested_output_old: Cell::new(0),
        };

        let poll0 = inner.poll_updated(&mut ctx);
        assert_eq!(poll0, Poll::Unchanged);
        assert!(inner.degraded_on_invalid);
        assert_eq!(called.get(), 0);

        ctx.phase = 1;
        let poll1 = inner.poll_updated(&mut ctx);
        assert_eq!(poll1, Poll::Updated);
        assert_eq!(inner.cached_output, 777);
        assert_eq!(called.get(), 1);
        assert_eq!(ctx.requested_output_new.get(), 1);
        assert_eq!(ctx.unrequested_output_old.get(), 1);
    }

    #[test]
    fn then_clone1_should_recover_after_input_dirty() {
        let _engine = Engine::new();

        let input: Anchor<u32, Engine> = Anchor::constant(1);
        let output: Anchor<u32, Engine> = Anchor::constant(2);
        let output_for_f = output.clone();

        struct Ctx {
            phase: u8,
            input_token: Token,
            output_token: Token,
            input_value: u32,
            output_value: u32,
            requested_input: Cell<u32>,
            requested_output: Cell<u32>,
        }

        impl crate::expert::UpdateContext for Ctx {
            type Engine = Engine;

            fn get<'out, 'slf, O: 'static>(&'slf self, anchor: &Anchor<O, Self::Engine>) -> &'out O
            where
                'slf: 'out,
            {
                use std::any::Any;

                let token = anchor.token();
                if token == self.input_token {
                    return (&self.input_value as &dyn Any)
                        .downcast_ref::<O>()
                        .expect("unexpected input type");
                }
                if token == self.output_token {
                    return (&self.output_value as &dyn Any)
                        .downcast_ref::<O>()
                        .expect("unexpected output type");
                }
                panic!("unexpected token requested: {token:?}");
            }

            fn request<O: 'static>(
                &mut self,
                anchor: &Anchor<O, Self::Engine>,
                _necessary: bool,
            ) -> Poll {
                let token = anchor.token();
                if token == self.input_token {
                    self.requested_input.set(self.requested_input.get() + 1);
                    return match self.phase {
                        0 => Poll::Unchanged,
                        1 => Poll::Updated,
                        _ => panic!("unexpected phase={}", self.phase),
                    };
                }
                if token == self.output_token {
                    self.requested_output.set(self.requested_output.get() + 1);
                    return match self.phase {
                        0 => Poll::PendingInvalidToken,
                        1 => Poll::Updated,
                        _ => panic!("unexpected phase={}", self.phase),
                    };
                }
                panic!("unexpected token requested: {token:?}");
            }

            fn unrequest<O: 'static>(&mut self, _anchor: &Anchor<O, Self::Engine>) {
                panic!("then_clone1 恢复测试不应调用 unrequest()");
            }

            fn dirty_handle(&mut self) -> <Self::Engine as crate::expert::Engine>::DirtyHandle {
                panic!("then_clone1 恢复测试不应申请 dirty_handle()");
            }
        }

        let mut inner = ThenClone1::<u32, u32, _, Engine> {
            f: move |_v: &u32| output_for_f.clone(),
            f_anchor: Some(output),
            cached_output: 42,
            degraded_on_invalid: false,
            output_stale: false,
            output_dirty: false,
            anchor: input,
            location: Location::caller(),
        };

        let mut ctx = Ctx {
            phase: 0,
            input_token: inner.anchor.token(),
            output_token: inner.f_anchor.as_ref().unwrap().token(),
            input_value: 1,
            output_value: 999,
            requested_input: Cell::new(0),
            requested_output: Cell::new(0),
        };

        let poll0 = inner.poll_updated(&mut ctx);
        assert_eq!(poll0, Poll::Unchanged);
        assert!(inner.degraded_on_invalid);

        let input_token = inner.anchor.token();
        inner.dirty(&input_token);
        assert!(!inner.degraded_on_invalid, "输入变动后应解除冻结");
        assert!(inner.output_stale, "输入变动后应标记 output_stale");

        ctx.phase = 1;
        let poll1 = inner.poll_updated(&mut ctx);
        assert_eq!(poll1, Poll::Updated);
        assert_eq!(inner.cached_output, 999);
        assert_eq!(ctx.requested_input.get(), 2);
        assert_eq!(ctx.requested_output.get(), 2);
    }
}

// 去重版仅生成 2、3 入参，用于 opt-in。
impl_tuple_then_dedupe! {
    [O0, 0]
    [O1, 1]
}

impl_tuple_then_dedupe! {
    [O0, 0]
    [O1, 1]
    [O2, 2]
}
