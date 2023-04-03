/*
 * @Author: Rais
 * @Date: 2023-03-23 10:44:30
 * @LastEditTime: 2023-04-03 23:23:41
 * @LastEditors: Rais
 * @Description:
 */
mod external_impl;
use tracing::{debug, trace};

use super::{
    Anchor, AnchorHandle, AnchorInner, DirtyHandle, Engine, OutputContext, Poll, UpdateContext, Var,
};

use std::{cell::RefCell, fmt::Debug};
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

pub struct VarVOA<T, E: Engine> {
    inner: Rc<RefCell<VarEAShared<T, E>>>,
    anchor: Anchor<T, E>,
}

impl<T, E: Engine> Eq for VarVOA<T, E> {}
impl<T, E: Engine> PartialEq for VarVOA<T, E> {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.inner, &other.inner) && self.anchor == other.anchor
    }
}
impl<T: Debug + 'static> Debug for VarVOA<T, crate::singlethread::Engine> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("Var({:?}) ", &*self.get()))
    }
}
impl<T: Display + 'static, E: Engine> Display for VarVOA<T, E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("Var({}) ", &*self.get()))
    }
}

impl<T, E: Engine> Clone for VarVOA<T, E> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            anchor: self.anchor.clone(),
        }
    }
}

pub fn voa<T, X: Into<T>, E: Engine>(x: X) -> ValOrAnchor<T, E> {
    ValOrAnchor::Val(x.into())
}

pub enum ValOrAnchor<T, E: Engine> {
    Val(T),
    Anchor(Anchor<T, E>),
}

impl<T, E: Engine> ValOrAnchor<T, E> {
    pub fn new_val(v: T) -> Self {
        Self::Val(v)
    }
}

impl<T: Default, E: Engine> Default for ValOrAnchor<T, E> {
    fn default() -> Self {
        Self::Val(T::default())
    }
}

impl<T: Eq, E: Engine> Eq for ValOrAnchor<T, E> {}

impl<T: PartialEq, E: Engine> PartialEq for ValOrAnchor<T, E> {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Val(l0), Self::Val(r0)) => l0 == r0,
            (Self::Anchor(l0), Self::Anchor(r0)) => l0 == r0,
            _ => false,
        }
    }
}

impl<T: PartialEq, E: Engine> PartialEq<T> for ValOrAnchor<T, E> {
    fn eq(&self, other: &T) -> bool {
        match self {
            ValOrAnchor::Val(v) => v == other,
            ValOrAnchor::Anchor(_) => false,
        }
    }
}

impl<T: PartialEq, E: Engine> PartialEq<Anchor<T, E>> for ValOrAnchor<T, E> {
    fn eq(&self, other: &Anchor<T, E>) -> bool {
        match self {
            ValOrAnchor::Val(_) => false,
            ValOrAnchor::Anchor(an) => an == other,
        }
    }
}
//@ ops ─────────────────────────────────────────────────────────────────────────────

impl<T: std::ops::Sub<Output = T>, E: Engine> core::ops::Sub<T> for ValOrAnchor<T, E> {
    type Output = Self;

    fn sub(self, rhs: T) -> Self::Output {
        match self {
            ValOrAnchor::Val(v) => ValOrAnchor::Val(v - rhs),
            ValOrAnchor::Anchor(_) => panic!("ValOrAnchor::Anchor sub not allowed"),
        }
    }
}

impl<T: std::ops::Add<Output = T>, E: Engine> core::ops::Add<T> for ValOrAnchor<T, E> {
    type Output = Self;

    fn add(self, rhs: T) -> Self::Output {
        match self {
            ValOrAnchor::Val(v) => ValOrAnchor::Val(v + rhs),
            ValOrAnchor::Anchor(_) => panic!("ValOrAnchor::Anchor add not allowed"),
        }
    }
}

impl<T, E: Engine> core::ops::AddAssign<T> for ValOrAnchor<T, E>
where
    T: core::ops::AddAssign<T>,
{
    fn add_assign(&mut self, rhs: T) {
        match self {
            ValOrAnchor::Val(t) => *t += rhs,
            ValOrAnchor::Anchor(_) => panic!("ValOrAnchor::Anchor add_assign not allowed"),
        }
    }
}
impl<T, E: Engine> core::ops::SubAssign<T> for ValOrAnchor<T, E>
where
    T: core::ops::SubAssign<T>,
{
    fn sub_assign(&mut self, rhs: T) {
        match self {
            ValOrAnchor::Val(t) => *t -= rhs,
            ValOrAnchor::Anchor(_) => panic!("ValOrAnchor::Anchor sub_assign not allowed"),
        }
    }
}

pub fn force_op<T, E: Engine>(voa: ValOrAnchor<T, E>) -> ForceOpVOA<T, E> {
    ForceOpVOA(voa)
}

pub struct ForceOpVOA<T, E: Engine>(ValOrAnchor<T, E>);

impl<
        T: std::ops::Sub<Output = T> + std::clone::Clone + std::cmp::PartialEq + 'static,
        E: Engine,
    > core::ops::Sub<T> for ForceOpVOA<T, E>
{
    type Output = ValOrAnchor<T, E>;

    fn sub(self, rhs: T) -> Self::Output {
        match self.0 {
            ValOrAnchor::Val(v) => ValOrAnchor::Val(v - rhs),
            ValOrAnchor::Anchor(an) => {
                ValOrAnchor::Anchor(an.map(move |x| x.clone() - rhs.clone()))
            }
        }
    }
}

impl<
        T: std::ops::Add<Output = T> + std::clone::Clone + std::cmp::PartialEq + 'static,
        E: Engine,
    > core::ops::Add<T> for ForceOpVOA<T, E>
{
    type Output = ValOrAnchor<T, E>;

    fn add(self, rhs: T) -> Self::Output {
        match self.0 {
            ValOrAnchor::Val(v) => ValOrAnchor::Val(v + rhs),
            ValOrAnchor::Anchor(an) => {
                ValOrAnchor::Anchor(an.map(move |x| x.clone() + rhs.clone()))
            }
        }
    }
}

impl<T, E: Engine> core::ops::AddAssign<T> for ForceOpVOA<T, E>
where
    T: core::ops::AddAssign<T>
        + std::cmp::PartialEq
        + Clone
        + std::ops::Add<T, Output = T>
        + 'static,
{
    fn add_assign(&mut self, rhs: T) {
        match &mut self.0 {
            ValOrAnchor::Val(t) => {
                *t += rhs;
            }
            ValOrAnchor::Anchor(an) => {
                *an = an.map(move |x| x.clone() + rhs.clone());
            }
        };
    }
}
impl<T, E: Engine> core::ops::SubAssign<T> for ForceOpVOA<T, E>
where
    T: core::ops::SubAssign<T>
        + std::cmp::PartialEq
        + Clone
        + std::ops::Sub<T, Output = T>
        + 'static,
{
    fn sub_assign(&mut self, rhs: T) {
        match &mut self.0 {
            ValOrAnchor::Val(t) => *t -= rhs,
            ValOrAnchor::Anchor(an) => {
                *an = an.map(move |x| x.clone() - rhs.clone());
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────

impl<T: Clone, E: Engine> Clone for ValOrAnchor<T, E> {
    fn clone(&self) -> Self {
        match self {
            Self::Val(arg0) => Self::Val(arg0.clone()),
            Self::Anchor(arg0) => Self::Anchor(arg0.clone()),
        }
    }
}

impl<T, E: Engine> ValOrAnchor<T, E> {
    /// Returns `true` if the either anchor is [`Val`].
    ///
    /// [`Val`]: EitherAnchor::Val
    #[must_use]
    pub fn is_val(&self) -> bool {
        matches!(self, Self::Val(..))
    }

    #[must_use]
    pub fn as_anchor(&self) -> Option<&Anchor<T, E>> {
        if let Self::Anchor(v) = self {
            Some(v)
        } else {
            None
        }
    }

    #[must_use]
    pub fn as_val(&self) -> Option<&T> {
        if let Self::Val(v) = self {
            Some(v)
        } else {
            None
        }
    }

    pub fn try_into_val(self) -> Result<T, Self> {
        if let Self::Val(v) = self {
            Ok(v)
        } else {
            Err(self)
        }
    }

    pub fn try_into_anchor(self) -> Result<Anchor<T, E>, Self> {
        if let Self::Anchor(v) = self {
            Ok(v)
        } else {
            Err(self)
        }
    }
}

impl<T: Debug, E: Engine> Debug for ValOrAnchor<T, E>
where
    <E as Engine>::AnchorHandle: std::fmt::Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Val(arg0) => f.debug_tuple("EitherAnchor::Val").field(arg0).finish(),
            Self::Anchor(arg0) => f.debug_tuple("EitherAnchor::Anchor").field(arg0).finish(),
        }
    }
}

impl<T: Display, E: Engine> Display for ValOrAnchor<T, E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Val(arg0) => f.write_fmt(format_args!("Val({})", arg0)),
            Self::Anchor(_arg0) => {
                f.write_fmt(format_args!("Anchor<{}>", std::any::type_name::<T>()))
            }
        }
    }
}

pub auto trait NotAnchorOrEA {}
impl<T, E> !NotAnchorOrEA for Anchor<T, E> {}
impl<T, E> !NotAnchorOrEA for ValOrAnchor<T, E> {}

// impl<T, X, E: Engine> From<X> for ValOrAnchor<T, E>
// where
//     T: NotAnchorOrEA,
//     X: Into<T> + NotAnchorOrEA,
// {
//     fn from(value: X) -> Self {
//         Self::Val(value.into())
//     }
// }
impl<T, E: Engine> From<T> for ValOrAnchor<T, E> {
    fn from(value: T) -> Self {
        Self::Val(value)
    }
}

impl<T, E: Engine> From<Anchor<T, E>> for ValOrAnchor<T, E> {
    fn from(value: Anchor<T, E>) -> Self {
        Self::Anchor(value)
    }
}

pub trait CastFromValOrAnchor<T>: Sized {
    fn cast_from(value: T) -> Self;
}
pub trait CastIntoValOrAnchor<T, E: Engine>: Sized {
    fn cast_into(self) -> ValOrAnchor<T, E>;
}
impl<W, T, E: Engine> CastIntoValOrAnchor<T, E> for W
where
    ValOrAnchor<T, E>: CastFromValOrAnchor<W>,
{
    #[inline]
    fn cast_into(self) -> ValOrAnchor<T, E> {
        ValOrAnchor::<T, E>::cast_from(self)
    }
}

impl<T, X, E: Engine> CastFromValOrAnchor<VarVOA<X, E>> for ValOrAnchor<T, E>
where
    X: Into<T> + Clone + 'static,
    T: PartialEq + 'static,
{
    fn cast_from(value: VarVOA<X, E>) -> Self {
        ValOrAnchor::Anchor(value.watch().map(|x| x.clone().into()))
    }
}
impl<T, X, E: Engine> CastFromValOrAnchor<Var<X, E>> for ValOrAnchor<T, E>
where
    X: Into<T> + Clone + 'static,
    T: PartialEq + 'static,
{
    fn cast_from(value: Var<X, E>) -> Self {
        ValOrAnchor::Anchor(value.watch().map(|x| x.clone().into()))
    }
}
impl<T, X, E: Engine> CastFromValOrAnchor<Anchor<X, E>> for ValOrAnchor<T, E>
where
    X: Into<T> + Clone + 'static,
    T: PartialEq + 'static,
{
    fn cast_from(value: Anchor<X, E>) -> Self {
        ValOrAnchor::Anchor(value.map(|x| x.clone().into()))
    }
}

impl<E: Engine, X, T> CastFromValOrAnchor<ValOrAnchor<X, E>> for ValOrAnchor<T, E>
where
    T: 'static + PartialEq,
    X: Into<T> + Clone + 'static,
{
    fn cast_from(value: ValOrAnchor<X, E>) -> Self {
        match value {
            ValOrAnchor::Val(v) => ValOrAnchor::Val(v.into()),
            ValOrAnchor::Anchor(av) => ValOrAnchor::Anchor(av.map(|x| x.clone().into())),
        }
    }
}

// default impl<T, X, E: Engine> FromValOrAnchor<ValOrAnchor<X, E>> for ValOrAnchor<T, E>
// where
//     T: NotAnchorOrEA + std::cmp::PartialEq + 'static,
//     X: NotAnchorOrEA + std::cmp::PartialEq + Into<T> + Clone + 'static,
// {
//     fn from_voa(value: ValOrAnchor<X, E>) -> Self {
//         println!(" use map");
//         match value {
//             ValOrAnchor::Val(x) => ValOrAnchor::Val(x.into()),
//             ValOrAnchor::Anchor(ax) => ValOrAnchor::Anchor(ax.map(|x| x.clone().into())),
//         }
//     }
// }

#[cfg(test)]
mod voa {
    use crate::{
        expert::CastIntoValOrAnchor,
        singlethread::{Engine, ValOrAnchor, Var},
    };

    #[test]
    fn test_into() {
        let _engine = Engine::new();
        let x = Var::new(1i32);
        let xw = x.watch();
        // let a = ValOrAnchor::Val(1i32);
        let n = 1i32;
        let _nn: i64 = n.into();

        let b: ValOrAnchor<i64> = xw.cast_into();
        // let b: ValOrAnchor<i64> = n.into();
        // let x = 1i32;
        // let y: i64 = x.into();
        // println!("y {:?}", y);
        println!("b {:?}", b);
    }
}

impl<T: 'static, E: Engine> VarVOA<T, E> {
    /// Creates a new Var
    pub fn new(val: impl Into<ValOrAnchor<T, E>>) -> VarVOA<T, E> {
        let val: Rc<ValOrAnchor<T, E>> = Rc::new(val.into());
        let inner = Rc::new(RefCell::new(VarEAShared {
            dirty_handle: None,
            val: val.clone(),
            output_stale: true,
        }));
        VarVOA {
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

        if let ValOrAnchor::<T, E>::Anchor(an) = &*inner.val {
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
                (ValOrAnchor::Val(_old), ValOrAnchor::Val(_new_v)) => {
                    debug!("a1");
                    if !Rc::ptr_eq(&self.val, &inner.val) {
                        Poll::Updated
                    } else {
                        force_unchanged_no_clone = true;
                        Poll::Unchanged
                    }
                }
                (ValOrAnchor::Val(_), ValOrAnchor::Anchor(new_a)) => {
                    debug!("a2");

                    ctx.request(new_a, true)
                }
                (ValOrAnchor::Anchor(outdated_anchor), ValOrAnchor::Val(_v)) => {
                    debug!("a3");

                    ctx.unrequest(outdated_anchor);
                    Poll::Updated
                }
                (ValOrAnchor::Anchor(outdated_anchor), ValOrAnchor::Anchor(new_a)) => {
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
                ValOrAnchor::Val(_) => Poll::Unchanged,
                ValOrAnchor::Anchor(an) => ctx.request(an, true),
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
            ValOrAnchor::Val(v) => v,
            ValOrAnchor::Anchor(an) => ctx.get(an),
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
            var_val_or_anchor::{ValOrAnchor, VarVOA},
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
        let a1 = VarVOA::new(1);

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
        let a1 = VarVOA::new(1);

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
