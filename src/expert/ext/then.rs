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
                    if std::env::var("ANCHORS_DEBUG_THEN")
                        .map(|v| v != "0")
                        .unwrap_or(false)
                    {
                        println!(
                            "THEN poll: output_stale={} f_anchor_present={} loc={:?}",
                            self.output_stale,
                            self.f_anchor.is_some(),
                            self.location
                        );
                    }
                    let mut found_pending = false;

                    $(
                        match ctx.request(&self.anchors.$num, true) {
                            Poll::Pending | Poll::PendingDefer | Poll::PendingInvalidToken => {
                                found_pending = true;
                                self.output_stale = true;
                            }
                            Poll::Updated => {
                                self.output_stale = true;
                            }
                            Poll::Unchanged => {
                                // do nothing
                            }
                        }
                    )+

                    if found_pending {
                        #[cfg(debug_assertions)]
                        {
                            if std::env::var("ANCHORS_DEBUG_THEN")
                                .map(|v| v != "0")
                                .unwrap_or(false)
                            {
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
                            if std::env::var("ANCHORS_DEBUG_THEN")
                                .map(|v| v != "0")
                                .unwrap_or(false)
                            {
                                tracing::warn!(
                                    target: "anchors",
                                    "then switching output anchor old={:?} new={:?} loc={:?}",
                                    outdated_anchor.token(),
                                    new_anchor.token(),
                                    self.location
                                );
                            }
                            if std::env::var("ANCHORS_DEBUG_THEN").map(|v| v != "0").unwrap_or(false) {
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
            match ctx.request(&self.anchor, true) {
                Poll::Pending | Poll::PendingDefer | Poll::PendingInvalidToken => {
                    self.output_stale = true;
                    return Poll::Pending;
                }
                Poll::Updated => {
                    self.output_stale = true;
                }
                Poll::Unchanged => {}
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
                    $(
                        match ctx.request(&self.anchors.$num, true) {
                            Poll::Pending | Poll::PendingDefer | Poll::PendingInvalidToken => {
                                found_pending = true;
                                self.output_stale = true;
                            }
                            Poll::Updated => {
                                self.output_stale = true;
                            }
                            Poll::Unchanged => {}
                        }
                    )+
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
