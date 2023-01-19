use tracing::{debug, trace};

use super::{
    Anchor, AnchorHandle, AnchorInner, DirtyHandle, Engine, OutputContext, Poll, UpdateContext,
};
use std::{cell::RefCell, fmt::Debug};
use std::{fmt::Display, rc::Rc};

/// An Anchor type for values that are mutated by calling a setter function from outside of the Anchors recomputation graph.
struct VarAnchor<T, E: Engine> {
    inner: Rc<RefCell<VarShared<T, E>>>,
    val: Rc<T>,
}

#[derive(Clone)]
struct VarShared<T, E: Engine> {
    dirty_handle: Option<E::DirtyHandle>,
    val: Rc<T>,
    value_changed: bool,
}

impl<T: Debug, E: Engine> Debug for VarShared<T, E>
where
    E::DirtyHandle: Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VarShared")
            .field("dirty_handle", &self.dirty_handle)
            .field("val", &self.val)
            .field("value_changed", &self.value_changed)
            .finish()
    }
}

/// A setter that can update values inside an associated `VarAnchor`.

pub struct Var<T, E: Engine> {
    inner: Rc<RefCell<VarShared<T, E>>>,
    anchor: Anchor<T, E>,
}
#[cfg(test)]
mod test {

    use crate::singlethread::*;

    #[test]
    fn look() {
        let mut engine = Engine::new();

        let a = Var::new(1);
        let aw = a.watch();
        let x = aw.map(|x| x + 1);
        println!("1.{a:#?}");
        engine.get(&x);
        println!("2.{a:#?}");
        a.set(2);
        println!("3.{a:#?}");
    }
}
impl<T, E: Engine> Eq for Var<T, E> {}
impl<T, E: Engine> PartialEq for Var<T, E> {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.inner, &other.inner) && self.anchor == other.anchor
    }
}
impl<T: Debug + 'static> Debug for Var<T, crate::singlethread::Engine> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("Var({:?}) ", &*self.get()))

        //NOTE debug
        // f.debug_struct("Var")
        //     .field("inner", &self.inner)
        //     .field("anchor", &self.anchor)
        //     .finish()
    }
}
impl<T: Display + 'static, E: Engine> Display for Var<T, E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("Var({}) ", &*self.get()))
    }
}

impl<T, E: Engine> Clone for Var<T, E> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            anchor: self.anchor.clone(),
        }
    }
}

impl<T: 'static, E: Engine> Var<T, E> {
    /// Creates a new Var
    pub fn new(val: T) -> Var<T, E> {
        let val = Rc::new(val);
        let inner = Rc::new(RefCell::new(VarShared {
            dirty_handle: None,
            val: val.clone(),
            value_changed: true,
        }));
        Var {
            inner: inner.clone(),
            anchor: E::mount(VarAnchor { inner, val }),
        }
    }

    // pub fn swap(&self, other: T) -> Rc<T> {
    //     let mut inner = self.inner.borrow_mut();
    //     let old = inner.val.clone();
    //     inner.val = Rc::new(other);
    //     if let None = &inner.dirty_handle {
    //         debug!( "===inner.dirty_handle None");
    //     }

    //     if let Some(waker) = &inner.dirty_handle {
    //         debug!( "===mark_dirty()");
    //         waker.mark_dirty();
    //     }

    //     inner.value_changed = true;
    //     old
    // }

    /// Updates the value inside the VarAnchor, and indicates to the recomputation graph that
    /// the value has changed.
    pub fn set(&self, val: T) {
        let mut inner = self.inner.borrow_mut();
        inner.val = Rc::new(val);
        // if let None = &inner.dirty_handle {
        //     debug!( "===inner.dirty_handle None");
        // }

        if let Some(waker) = &inner.dirty_handle {
            // debug!( "===mark_dirty()");
            waker.mark_dirty();
        }

        inner.value_changed = true;
    }

    /// Retrieves the last value set
    pub fn get(&self) -> Rc<T> {
        self.inner.borrow().val.clone()
    }

    pub fn watch(&self) -> Anchor<T, E> {
        self.anchor.clone()
    }
}

impl<E: Engine, T: 'static> AnchorInner<E> for VarAnchor<T, E> {
    type Output = T;
    fn dirty(&mut self, _edge: &<E::AnchorHandle as AnchorHandle>::Token) {
        panic!("somehow an input was dirtied on VarAnchor; it never has any inputs to dirty")
    }

    fn poll_updated<G: UpdateContext<Engine = E>>(&mut self, ctx: &mut G) -> Poll {
        trace!("poll_updated");
        let mut inner = self.inner.borrow_mut();
        let first_update = inner.dirty_handle.is_none();
        if first_update {
            inner.dirty_handle = Some(ctx.dirty_handle());
        }
        let res = if inner.value_changed {
            self.val = inner.val.clone();
            Poll::Updated
        } else {
            Poll::Unchanged
        };
        inner.value_changed = false;
        res
    }

    fn output<'slf, 'out, G: OutputContext<'out, Engine = E>>(
        &'slf self,
        _ctx: &mut G,
    ) -> &'out Self::Output
    where
        'slf: 'out,
    {
        &self.val
    }
}
