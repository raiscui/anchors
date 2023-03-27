/*
 * @Author: Rais
 * @Date: 2023-03-23 10:44:30
 * @LastEditTime: 2023-03-26 23:13:06
 * @LastEditors: Rais
 * @Description:
 */

use tracing::{debug, debug_span, trace, warn};

use super::{
    Anchor, AnchorHandle, AnchorInner, DirtyHandle, Engine, OutputContext, Poll, UpdateContext,
};
use crate::singlethread::AnchorToken;
use std::{any::Any, cell::RefCell, fmt::Debug};
use std::{fmt::Display, rc::Rc};

/// An Anchor type for values that are mutated by calling a setter function from outside of the Anchors recomputation graph.
struct VarEitherAnchor<T, E: Engine> {
    inner: Rc<RefCell<VarEAShared<T, E>>>,
    val: Rc<ValOrAnchor<T, E>>,
}

#[derive(Clone)]
struct VarEAShared<T, E: Engine> {
    dirty_handle: Option<E::DirtyHandle>,
    val: Rc<ValOrAnchor<T, E>>,
    output_stale: bool,
}

impl<T: Debug, E: Engine> Debug for VarEAShared<T, E>
where
    E::DirtyHandle: Debug,
    <E as Engine>::AnchorHandle: std::fmt::Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VarShared")
            .field("dirty_handle", &self.dirty_handle)
            .field("val", &self.val)
            .field("value_changed", &self.output_stale)
            .finish()
    }
}

/// A setter that can update values inside an associated `VarAnchor`.

pub struct VarEA<T, E: Engine> {
    inner: Rc<RefCell<VarEAShared<T, E>>>,
    anchor: Anchor<T, E>,
}

impl<T, E: Engine> Eq for VarEA<T, E> {}
impl<T, E: Engine> PartialEq for VarEA<T, E> {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.inner, &other.inner) && self.anchor == other.anchor
    }
}
impl<T: Debug + 'static> Debug for VarEA<T, crate::singlethread::Engine> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("Var({:?}) ", &*self.get()))
    }
}
impl<T: Display + 'static, E: Engine> Display for VarEA<T, E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("Var({}) ", &*self.get()))
    }
}

impl<T, E: Engine> Clone for VarEA<T, E> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            anchor: self.anchor.clone(),
        }
    }
}

pub enum ValOrAnchor<T, E: Engine> {
    EVal(T),
    EAnchor(Anchor<T, E>),
}

impl<T: Clone, E: Engine> Clone for ValOrAnchor<T, E> {
    fn clone(&self) -> Self {
        match self {
            Self::EVal(arg0) => Self::EVal(arg0.clone()),
            Self::EAnchor(arg0) => Self::EAnchor(arg0.clone()),
        }
    }
}

impl<T, E: Engine> ValOrAnchor<T, E> {
    /// Returns `true` if the either anchor is [`Val`].
    ///
    /// [`Val`]: EitherAnchor::EVal
    #[must_use]
    pub fn is_val(&self) -> bool {
        matches!(self, Self::EVal(..))
    }

    #[must_use]
    pub fn as_anchor(&self) -> Option<&Anchor<T, E>> {
        if let Self::EAnchor(v) = self {
            Some(v)
        } else {
            None
        }
    }
}

impl<T: Debug, E: Engine> Debug for ValOrAnchor<T, E>
where
    <E as Engine>::AnchorHandle: std::fmt::Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EVal(arg0) => f.debug_tuple("EitherAnchor::EVal").field(arg0).finish(),
            Self::EAnchor(arg0) => f.debug_tuple("EitherAnchor::EAnchor").field(arg0).finish(),
        }
    }
}

impl<T: Display, E: Engine> Display for ValOrAnchor<T, E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EVal(arg0) => f.write_fmt(format_args!("Val({})", arg0)),
            Self::EAnchor(_arg0) => {
                f.write_fmt(format_args!("Anchor<{}>", std::any::type_name::<T>()))
            }
        }
    }
}
pub auto trait NotAnchorOrEA {}
impl<T, E> !NotAnchorOrEA for Anchor<T, E> {}
impl<T, E> !NotAnchorOrEA for ValOrAnchor<T, E> {}

impl<T, E: Engine> From<T> for ValOrAnchor<T, E>
where
    T: NotAnchorOrEA,
{
    fn from(value: T) -> Self {
        Self::EVal(value)
    }
}

impl<T, E: Engine> From<Anchor<T, E>> for ValOrAnchor<T, E> {
    fn from(value: Anchor<T, E>) -> Self {
        Self::EAnchor(value)
    }
}

impl<T: 'static, E: Engine> VarEA<T, E> {
    /// Creates a new Var
    pub fn new(val: impl Into<ValOrAnchor<T, E>>) -> VarEA<T, E> {
        let val: Rc<ValOrAnchor<T, E>> = Rc::new(val.into());
        let inner = Rc::new(RefCell::new(VarEAShared {
            dirty_handle: None,
            val: val.clone(),
            output_stale: true,
        }));
        VarEA {
            inner: inner.clone(),
            anchor: E::mount(VarEitherAnchor { inner, val }),
        }
    }

    /// Updates the value inside the VarAnchor, and indicates to the recomputation graph that
    /// the value has changed.
    pub fn set(&self, val: impl Into<ValOrAnchor<T, E>>) {
        let mut inner = self.inner.borrow_mut();
        inner.val = Rc::new(val.into());
        // if let None = &inner.dirty_handle {
        //     debug!( "===inner.dirty_handle None");
        // }
        // debug!("set0");

        if let Some(waker) = &inner.dirty_handle {
            // debug!( "===mark_dirty()");

            waker.mark_dirty();
        }
        // debug!("set2");

        inner.output_stale = true;
    }

    /// Retrieves the last value set
    pub fn get(&self) -> Rc<ValOrAnchor<T, E>> {
        self.inner.borrow().val.clone()
    }

    pub fn watch(&self) -> Anchor<T, E> {
        self.anchor.clone()
    }
}

impl<E: Engine, T: 'static> AnchorInner<E> for VarEitherAnchor<T, E> {
    type Output = T;
    fn dirty(&mut self, edge: &<E::AnchorHandle as AnchorHandle>::Token) {
        // debug!("dirty...");
        let mut inner = self.inner.borrow_mut();

        // inner.output_stale = true;

        if let ValOrAnchor::<T, E>::EAnchor(an) = &*inner.val {
            if &an.token() == edge {
                // debug!("dirty...output_stale=true");
                inner.output_stale = true;
            }
        }
    }

    fn poll_updated<G: UpdateContext<Engine = E>>(&mut self, ctx: &mut G) -> Poll {
        trace!("poll_updated");
        let mut inner = self.inner.borrow_mut();

        if inner.output_stale {
            let first = inner.dirty_handle.is_none();
            if first {
                inner.dirty_handle = Some(ctx.dirty_handle());
            }
            debug!("a0");
            let mut force_unchanged_no_clone = false;
            let poll = match (&*self.val, &*inner.val) {
                (ValOrAnchor::EVal(_old), ValOrAnchor::EVal(_new_v)) => {
                    debug!("a1");
                    if !Rc::ptr_eq(&self.val, &inner.val) {
                        Poll::Updated
                    } else {
                        force_unchanged_no_clone = true;
                        Poll::Unchanged
                    }
                }
                (ValOrAnchor::EVal(_), ValOrAnchor::EAnchor(new_a)) => {
                    debug!("a2");

                    ctx.request(new_a, true)
                }
                (ValOrAnchor::EAnchor(outdated_anchor), ValOrAnchor::EVal(_v)) => {
                    debug!("a3");

                    ctx.unrequest(outdated_anchor);
                    Poll::Updated
                }
                (ValOrAnchor::EAnchor(outdated_anchor), ValOrAnchor::EAnchor(new_a)) => {
                    debug!("a4");

                    if outdated_anchor != new_a {
                        ctx.unrequest(outdated_anchor);

                        ctx.request(new_a, true)
                    } else {
                        //same
                        match ctx.request(new_a, true) {
                            Poll::Unchanged => {
                                force_unchanged_no_clone = true;
                                Poll::Unchanged
                            }
                            up_or_pending => up_or_pending,
                        }
                    }
                }
            };
            if poll == Poll::Pending {
                debug!("a pending");
                return Poll::Pending;
            }
            inner.output_stale = false;

            if force_unchanged_no_clone {
                Poll::Unchanged
            } else {
                // NOTE 不能直接反馈 poll, 因为有可能 new anchor 自上次以来并没有变更,但是 anchor是新赋予此VarEitherAnchor 的,
                // NOTE 如果反馈 new anchor unchanged 状态, 下层会不更新 此 VarEitherAnchor 的历史值
                debug!("a up-- val set up");

                self.val = Rc::clone(&inner.val);

                Poll::Updated
            }
        } else {
            debug!("in update - unchanged");

            match &*inner.val {
                ValOrAnchor::EVal(_) => Poll::Unchanged,
                ValOrAnchor::EAnchor(an) => ctx.request(an, true),
            }
        }
    }

    fn output<'slf, 'out, G: OutputContext<'out, Engine = E>>(
        &'slf self,
        ctx: &mut G,
    ) -> &'out Self::Output
    where
        'slf: 'out,
    {
        match &*self.val {
            ValOrAnchor::EVal(v) => v,
            ValOrAnchor::EAnchor(an) => ctx.get(an),
        }
    }
}

#[cfg(test)]
mod tests {

    fn tracing_init() {
        use tracing_subscriber::prelude::*;
        // A general-purpose logging layer.
        let fmt_layer = tracing_subscriber::fmt::layer();

        // Build a subscriber that combines the access log and stdout log
        // layers.
        tracing_subscriber::registry()
            .with(fmt_layer)
            .try_init()
            .ok();
    }
    use tracing::debug;

    use crate::{
        expert::{
            var_either_anchor::{ValOrAnchor, VarEA},
            Constant,
        },
        singlethread::{Engine, Var},
    };
    // #[test]
    // fn test_var_mem() {
    //     let mut engine = Engine::new();

    //     let mut f = || {
    //         // 创建锚点 a 和 b
    //         let a1 = Var::new(1);
    //         let aw = a1.watch();
    //         let awm = aw.map(|x| x + 1);
    //         let x = engine.get(&awm);
    //     };
    //     for x in 0..100000000 {
    //         f();
    //     }
    // }

    #[test]
    fn test_var_either2() {
        let mut engine = Engine::new();
        // 创建锚点 a 和 b
        let a1 = VarEA::new(1);

        let aw = a1.watch().map(|x| x + 1);

        // debug!("{}", engine.get(&aw));
        assert_eq!(engine.get(&aw), 2)
    }

    #[test]
    fn test_var_either() {
        // tracing_init();
        let mut engine = Engine::new();
        let _xx: ValOrAnchor<i32, Engine> = 2.into();
        let _xx: ValOrAnchor<i32, Engine> = Var::new(2).watch().into();
        // 创建锚点 a 和 b
        let a1 = VarEA::new(1);

        let aw = a1.watch();

        debug!("{}", engine.get(&aw));
        let c1 = Constant::new_internal(2);
        a1.set(c1);
        debug!("{}", engine.get(&aw));
        assert_eq!(engine.get(&aw), 2);
        let c2 = Var::new(3);
        a1.set(c2.watch());
        debug!("{}", engine.get(&aw));
        // ─────────────────────────────────────────────────────────────

        debug!("step --- 0");

        assert_eq!(engine.get(&aw), 3);
        debug!("step --- 1");

        c2.set(4);
        debug!("step --- 2");

        debug!("{}", engine.get(&aw));
        debug!("step --- 3");

        assert_eq!(engine.get(&aw), 4);
        a1.set(5);
        debug!("{}", engine.get(&aw));

        assert_eq!(engine.get(&aw), 5);
        debug!("step --- 4");

        debug!("{}", engine.get(&aw));
        debug!("{}", engine.get(&aw));
    }
}
