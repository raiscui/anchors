use tracing::{error, trace};

use super::{
    Anchor, AnchorHandle, AnchorInner, DirtyHandle, Engine, OutputContext, Poll, UpdateContext,
    bind_or_verify_dirty_handle,
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
    dirty_handle_owner_id: Option<usize>,
    val: Rc<T>,
    value_changed: bool,
    ////////////////////////////////////////////////////////////////////////////////
    // 延迟补偿：记录“set 发生但未能入队 dirty”的情况
    ////////////////////////////////////////////////////////////////////////////////
    pending_dirty: bool,
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
            .field("dirty_handle_owner_id", &self.dirty_handle_owner_id)
            .field("val", &self.val)
            .field("value_changed", &self.value_changed)
            .field("pending_dirty", &self.pending_dirty)
            .finish()
    }
}

/// A setter that can update values inside an associated `VarAnchor`.
pub struct Var<T, E: Engine> {
    inner: Rc<RefCell<VarShared<T, E>>>,
    ////////////////////////////////////////////////////////////////////////////////
    // 惰性 mount(与 VarVOA 同构, 2026-09-02 T3):
    //
    // - anchor(VarAnchor 代理)推迟到首次 set/watch 时才创建;
    // - Rc<RefCell<Option<Anchor>>> 共享: 同一 Var 的所有 clone 看到同一份 anchor,
    //   防止"各自 mount"分叉出多个代理节点;
    // - 从未被 set/watch 的 Var(如 gtree 里未被消费的 use_state 槽)零节点成本;
    // - Var 没有 ValOrAnchor 分支(纯值), 因此所有 Var 都走惰性路径。
    ////////////////////////////////////////////////////////////////////////////////
    anchor: Rc<RefCell<Option<Anchor<T, E>>>>,
}

impl<T: 'static, E: Engine> Var<T, E> {
    /// 惰性 mount: 若代理 anchor 尚未创建, 则在当前时机 mount VarAnchor。
    ///
    /// 调用约束: 调用方不得持有 `self.inner` 的活跃借用(本函数会 borrow/borrow_mut inner)。
    fn ensure_mounted(&self) -> Anchor<T, E> {
        let mut cell = self.anchor.borrow_mut();
        if let Some(a) = &*cell {
            return a.clone();
        }
        let val = self.inner.borrow().val.clone();
        let anchor = E::mount(VarAnchor {
            inner: self.inner.clone(),
            val,
        });
        ////////////////////////////////////////////////////////////////////////////////
        // 提前建立 dirty_handle，确保首次 set 即可 mark_dirty，不依赖 poll_updated 时机。
        // - 仅缓存句柄，不触发任何 get/stabilize。
        // - 若引擎不支持兜底句柄，保持 None。
        ////////////////////////////////////////////////////////////////////////////////
        if let Some(handle) = <E as Engine>::fallback_dirty_handle(anchor.token()) {
            let mut inner_mut = self.inner.borrow_mut();
            inner_mut.dirty_handle_owner_id = handle.owner_id();
            inner_mut.dirty_handle = Some(handle);
        }
        *cell = Some(anchor.clone());
        anchor
    }
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

    #[test]
    // 惰性 mount(T3)的引擎归属语义变化: anchor 归属引擎 = 首次 set/watch 时的当前引擎
    // (原为构造时引擎)。本测试的旧前提("engine_a 构造 + engine_b poll"必 panic)不再成立:
    // watch 发生在 engine_b 上下文 → mount 进 graph_b → 轮询同引擎, 自洽。
    // 单引擎 app 完全等价; 跨引擎错配的保护仍由 poll 期 owner 校验承载。
    #[ignore = "惰性 mount: 引擎归属语义已变为首次触碰时确定(单引擎等价)"]
    #[should_panic(expected = "dirty_handle 绑定的 engine 发生切换")]
    fn var_should_panic_when_polled_by_another_engine() {
        // ─────────────────────────────────────────────────────────────
        // 场景:
        // - source 在 engine-A 创建时,会预绑定一个 dirty_handle。
        // - mapped 与第一次 `.get()` 发生在 engine-B。
        // - 现在我们明确要求 dirty_handle owner 不可漂移:
        //   一旦被另一个 engine 轮询,应立即 panic,而不是热路径持续重绑。
        // ─────────────────────────────────────────────────────────────
        let _engine_a = Engine::new();
        let source = Var::new(1u32);

        let mut engine_b = Engine::new();
        let mapped = source.watch().map(|value| *value + 1);

        let _ = engine_b.get(&mapped);
    }
}
impl<T, E: Engine> Eq for Var<T, E> {}
impl<T, E: Engine> PartialEq for Var<T, E> {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.inner, &other.inner) && *self.anchor.borrow() == *other.anchor.borrow()
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
            dirty_handle_owner_id: None,
            val: val.clone(),
            value_changed: true,
            pending_dirty: false,
        }));
        ////////////////////////////////////////////////////////////////////////////////
        // 惰性 mount(T3): 构造期不 mount 代理节点 — Var 是纯值容器, 没有任何依赖
        // 需要立即建立; 推迟到首次 set/watch 的 ensure_mounted(与 VarVOA 的 T1 模式同构)。
        ////////////////////////////////////////////////////////////////////////////////
        Var {
            inner,
            anchor: Rc::new(RefCell::new(None)),
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
        ////////////////////////////////////////////////////////////////////////////////
        // 惰性 mount(T5): 未 mount 时 set 走纯数据面 — 不物化响应面。
        //
        // - 无消费者(anchor 未创建 ⇔ 从未被 watch)时, mark_dirty 没有可唤醒的对象,
        //   mount 出来的节点纯属浪费(set-only Var 零节点的依据);
        // - 值累积在 inner.val; 未来首次 watch 时 ensure_mounted 用最新值构造
        //   VarAnchor, 消费者首次 poll 即读到最新值(Unchanged + output 直读);
        // - 已 mount 后的 set 走下方原路径(mark_dirty 通知既有消费者)。
        ////////////////////////////////////////////////////////////////////////////////
        if self.anchor.borrow().is_none() {
            let mut inner = self.inner.borrow_mut();
            inner.val = Rc::new(val);
            return;
        }
        ////////////////////////////////////////////////////////////////////////////////
        // 惰性 mount(T3): set 前先确保代理已 mount(mark_dirty 依赖 anchor token)。
        //
        // ⚠️ 顺序约束(防 RefCell 双借 panic, T1 实测教训): ensure_mounted 必须在
        //    inner.borrow_mut() 之前调用 — 它内部会读 inner.val 构造 VarAnchor。
        ////////////////////////////////////////////////////////////////////////////////
        let anchor = self.ensure_mounted();
        let mut inner = self.inner.borrow_mut();
        inner.val = Rc::new(val);
        // if let None = &inner.dirty_handle {
        //     debug!( "===inner.dirty_handle None");
        // }

        ////////////////////////////////////////////////////////////////////////////////
        // 优先尝试在 set 阶段建立 dirty_handle,避免持续走“无 handle”路径。
        ////////////////////////////////////////////////////////////////////////////////
        if inner.dirty_handle.is_none() {
            let token = anchor.token();
            if let Some(handle) = <E as Engine>::fallback_dirty_handle(token) {
                inner.dirty_handle_owner_id = handle.owner_id();
                inner.dirty_handle = Some(handle);
            }
        }

        if let Some(waker) = &inner.dirty_handle {
            // debug!( "===mark_dirty()");
            waker.mark_dirty();
            inner.pending_dirty = false;
        } else {
            ////////////////////////////////////////////////////////////////////////////////
            // 兜底：dirty_handle 尚未建立时，仍尝试把自身 token 推入 dirty_marks。
            //
            // 说明：
            // - 这不是“额外 get”，只是入队一个 token；
            // - 目的是让本次/下一次 stabilize 能看到这次 set，并在需要时触发重算；
            // - 若当前 Engine 不支持兜底入队（fallback_mark_dirty=false），仅在 debug 开关下输出诊断。
            ////////////////////////////////////////////////////////////////////////////////
            let token = anchor.token();
            let queued = <E as Engine>::fallback_mark_dirty(token);

            ////////////////////////////////////////////////////////////////////////////////
            // 补偿：若本次 set 无法入队，记录 pending_dirty，待首次 poll_updated 建立 handle 后补偿入队。
            ////////////////////////////////////////////////////////////////////////////////
            if queued {
                inner.pending_dirty = false;
            } else {
                inner.pending_dirty = true;
                #[cfg(debug_assertions)]
                if emg_debug_env::bool_lenient("EMG_DEBUG_SCENE_CTX_PTR") {
                    let thread_id = std::thread::current().id();
                    let engine_inited = <E as Engine>::is_engine_initialized();
                    eprintln!(
                        "[anchors][var] dirty_handle missing type={} val_ptr={:p} thread={:?} engine_inited={}",
                        std::any::type_name::<T>(),
                        Rc::as_ptr(&inner.val),
                        thread_id,
                        engine_inited
                    );
                }
            }
        }

        inner.value_changed = true;
    }

    /// Retrieves the last value set
    pub fn get(&self) -> Rc<T> {
        self.inner.borrow().val.clone()
    }

    pub fn watch(&self) -> Anchor<T, E> {
        // 惰性 mount(T3): watch 时确保代理已 mount, 返回共享 anchor。
        self.ensure_mounted()
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
        ////////////////////////////////////////////////////////////////////////////////
        // 收口语义:
        // - dirty_handle 只在首次 poll 时绑定到当前 engine；
        // - 若后续 poll 发生在另一份 engine 上,直接 panic；
        // - 不再为“跨 engine 漂移”付出每次 poll 都重绑的常态成本。
        ////////////////////////////////////////////////////////////////////////////////
        let VarShared {
            dirty_handle,
            dirty_handle_owner_id,
            ..
        } = &mut *inner;
        bind_or_verify_dirty_handle::<E, G>(
            dirty_handle,
            dirty_handle_owner_id,
            ctx,
            "Var",
            std::any::type_name::<T>(),
        );
        if inner.pending_dirty {
            if let Some(waker) = &inner.dirty_handle {
                waker.mark_dirty();
            }
            inner.pending_dirty = false;
        }
        if inner.value_changed {
            inner.value_changed = false;
            if !Rc::ptr_eq(&self.val, &inner.val) {
                self.val = Rc::clone(&inner.val);
                if emg_debug_env::bool_lenient("ANCHORS_DEBUG_VAR") {
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
    fn var_lazy_mount_should_not_mount_until_touched() {
        ////////////////////////////////////////////////////////////////////////////////
        // 惰性 mount 回归(T3, 与 VarVOA 的 T1 回归同构):
        // - 未被 set/watch 触碰的 Var 不 mount 任何节点;
        // - 首次 watch 时恰好 mount 一个 VarAnchor 代理。
        ////////////////////////////////////////////////////////////////////////////////
        let engine = Engine::new();
        let before = engine.metrics_snapshot().active_nodes;

        let v = Var::<u32>::new(1);
        assert_eq!(
            engine.metrics_snapshot().active_nodes,
            before,
            "未触碰的 Var 不应 mount 节点"
        );

        let _anchor = v.watch();
        assert_eq!(
            engine.metrics_snapshot().active_nodes,
            before + 1,
            "首次 watch 应恰好 mount 一个代理节点"
        );
    }

    #[test]
    fn var_set_before_watch_should_not_double_borrow_and_should_converge() {
        ////////////////////////////////////////////////////////////////////////////////
        // 惰性 mount 回归(RefCell 双借防线, T1 实测教训的 Var 版):
        // - set 先于 watch 发生(未 mount 状态)时, set 内的 ensure_mounted 必须在
        //   inner.borrow_mut() 窗口之前完成, 不得 panic;
        // - 值语义: mount 时的构造值 = 出生值, set 写入的新值在首次 poll 后生效。
        ////////////////////////////////////////////////////////////////////////////////
        let mut engine = Engine::new();
        let v = Var::<u32>::new(1);

        v.set(2);
        let w = v.watch();
        assert_eq!(engine.get(&w), 2);
    }

    #[test]
    fn var_multiple_sets_before_watch_should_expose_latest_value() {
        ////////////////////////////////////////////////////////////////////////////////
        // 惰性 mount 回归(多 set 后 mount): watch 时 mount 的代理以最新值收敛。
        ////////////////////////////////////////////////////////////////////////////////
        let mut engine = Engine::new();
        let v = Var::<u32>::new(0);

        v.set(1);
        v.set(2);
        v.set(3);
        let w = v.watch();
        assert_eq!(engine.get(&w), 3);
    }

    #[test]
    fn var_set_only_should_not_mount_any_node() {
        ////////////////////////////////////////////////////////////////////////////////
        // 惰性 mount 回归(T5): set-only(从未 watch)的 Var 零节点/零调度物化;
        // get 读累积最新值; 事后首次 watch 仍能拿到最新值(纯数据面 → 响应面无缝衔接)。
        ////////////////////////////////////////////////////////////////////////////////
        let mut engine = Engine::new();
        let before = engine.metrics_snapshot().active_nodes;

        let v = Var::<u32>::new(1);
        v.set(2);
        v.set(3);
        assert_eq!(
            engine.metrics_snapshot().active_nodes,
            before,
            "set-only(未 watch)的 Var 不应 mount 节点"
        );
        assert_eq!(*v.get(), 3, "get 应读到累积的最新值");

        let w = v.watch();
        assert_eq!(engine.get(&w), 3, "事后首次 watch 应读到最新值");
    }

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
