use tracing::{error, trace};

use super::{
    Anchor, AnchorHandle, AnchorInner, DirtyHandle, Engine, OutputContext, Poll, UpdateContext,
};
use std::{any::Any, cell::RefCell, fmt::Debug};
use std::{fmt::Display, rc::Rc};

/// An Anchor type for values that are mutated by calling a setter function from outside of the Anchors recomputation graph.
struct VarAnchor<T, E: Engine> {
    inner: Rc<RefCell<VarShared<T, E>>>,
    val: Rc<T>,
}

// impl<T, E: Engine> Drop for VarAnchor<T, E> {
//     fn drop(&mut self) {
//         let _span = trace_span!("anchors-drop").entered();
//         trace!("drop VarAnchor<{}>", std::any::type_name::<T>());
//     }
// }

#[derive(Clone)]
struct VarShared<T, E: Engine> {
    dirty_handle: Option<E::DirtyHandle>,
    val: Rc<T>,
    value_changed: bool,
}

// impl<T, E: Engine> Drop for VarShared<T, E> {
//     fn drop(&mut self) {
//         let _span = trace_span!("anchors-drop").entered();
//         trace!("drop VarShared<{}>", std::any::type_name::<T>());
//     }
// }

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

// impl<T, E: Engine> Drop for Var<T, E> {
//     fn drop(&mut self) {
//         let _span = trace_span!("anchors-drop").entered();
//         trace!("drop Var<{}>", std::any::type_name::<T>());
//     }
// }
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
    fn dirty(&mut self, edge: &<E::AnchorHandle as AnchorHandle>::Token) {
        let e = edge as &dyn Any;
        let nodekey = e
            .downcast_ref::<crate::singlethread::AnchorToken>()
            .unwrap();
        let ng = unsafe { nodekey.ptr.lookup_unchecked() };

        error!(
            target:"anchors",
            "dirty,  edge: {:?},edge info: {:?}  ,type: {:?}",
            edge,
            ng.debug_info.get(),
            std::any::type_name::<T>(),
        );
    }

    fn poll_updated<G: UpdateContext<Engine = E>>(&mut self, ctx: &mut G) -> Poll {
        trace!("poll_updated");
        let mut inner = self.inner.borrow_mut();
        let first_update = inner.dirty_handle.is_none();
        if first_update {
            inner.dirty_handle = Some(ctx.dirty_handle());
        }
        if inner.value_changed {
            inner.value_changed = false;
            if !Rc::ptr_eq(&self.val, &inner.val) {
                self.val = Rc::clone(&inner.val);
                if std::env::var("ANCHORS_DEBUG_VAR")
                    .map(|v| v != "0")
                    .unwrap_or(false)
                {
                    println!("VAR 更新 type={}", std::any::type_name::<T>());
                }
                Poll::Updated
            } else {
                Poll::Unchanged
            }
        } else {
            Poll::Unchanged
        }
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

// impl<E: Engine, K, V, S> AnchorInner<E> for VarAnchor<HashMap<K, V, S>, E> {
//     type Output = HashMap<K, V, S>;
//     fn dirty(&mut self, edge: &<E::AnchorHandle as AnchorHandle>::Token) {
//         let _span = debug_span!("anchors-dirty").entered();
//         let e = edge as &dyn Any;
//         let ee = e.downcast_ref::<AnchorToken>().unwrap();
//         let ng = unsafe { ee.ptr.lookup_unchecked() };
//         warn!(
//             "VarAnchor dirty, debug_info=\n=========={:?}\ntype T:\n=========={}",
//             ng.debug_info.get(),
//             std::any::type_name::<HashMap<K, V, S>>()
//         );

//         panic!("somehow an input was dirtied on VarAnchor; it never has any inputs to dirty")
//     }

//     fn poll_updated<G: UpdateContext<Engine = E>>(&mut self, ctx: &mut G) -> Poll {
//         trace!("poll_updated");
//         let mut inner = self.inner.borrow_mut();
//         let first_update = inner.dirty_handle.is_none();
//         if first_update {
//             inner.dirty_handle = Some(ctx.dirty_handle());
//         }
//         let res = if inner.value_changed {
//             self.val = inner.val.clone();
//             Poll::Updated
//         } else {
//             Poll::Unchanged
//         };
//         inner.value_changed = false;
//         res
//     }

//     fn output<'slf, 'out, G: OutputContext<'out, Engine = E>>(
//         &'slf self,
//         _ctx: &mut G,
//     ) -> &'out Self::Output
//     where
//         'slf: 'out,
//     {
//         &self.val
//     }
// }
#[cfg(test)]
mod tests {
    use crate::{
        expert::{Constant, MultiAnchor},
        singlethread::{Engine, Var},
    };

    #[test]
    fn test_mut() {
        let mut engine = Engine::new();

        let a = Var::new(1);

        let aw = a.watch().map_mut(0i32, |out, &x| {
            if x > 5 {
                println!("change, out 2");
                *out = 2;
                true
            } else {
                println!("not change, out 1");
                *out = 1;
                false
            }
        });

        let b = Var::new(2);

        let aw2 = (&b.watch(), &aw).map(|bv, av| {
            println!("bv: {bv} av: {av}");
            *av
        });

        let aw_solo = aw.map(|av| {
            println!(" aw change : {av}");
            *av
        });

        println!("--------------------- {}", engine.get(&aw));
        println!("-- {}", engine.get(&aw2));
        println!("--s {}", engine.get(&aw_solo));
        // ─────────────────────────────────────────────────────────────

        a.set(10);
        println!("--------------------- {}", engine.get(&aw));
        println!("--s {}", engine.get(&aw_solo));
        println!("-- {}", engine.get(&aw2));

        a.set(2);
        println!("--------------------- {}", engine.get(&aw));
        println!("--s {}", engine.get(&aw_solo));
        println!("-- {}", engine.get(&aw2));
        println!("--------------------- {}", engine.get(&aw));
        println!("--s {}", engine.get(&aw_solo));
        println!("-- {}", engine.get(&aw2));
        //TODO fix 这里 --s solo 是 2 但是 aw2是1 ,(&b.watch(), &aw).map 和 aw.map 结果不一致
    }

    #[test]
    fn test_var_eq() {
        let mut engine = Engine::new();

        // 创建锚点 a 和 b
        let a1 = Constant::new_internal(1);
        let a2 = Constant::new_internal(2);
        let aa = Var::new(a1);
        let aw = aa.watch().then(|an| an.clone());
        let aw2 = aa.watch().map(|an| an.map(|x| x + 1));
        let aw3 = aa.watch().then(|an| an.clone());
        println!("{}", engine.get(&aw));
        let x = engine.get(&aw2);
        let x = engine.get(&x);
        println!("{}", x);
        println!("{}", engine.get(&aw3));
        aa.set(a2);
        println!("{}", engine.get(&aw));
        let x = engine.get(&aw2);
        let x = engine.get(&x);
        println!("{}", x);
        println!("{}", engine.get(&aw3));
    }
}
