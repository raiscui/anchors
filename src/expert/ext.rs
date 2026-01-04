use super::{Anchor, AnchorInner, Engine};
use std::panic::Location;

pub mod cutoff;
pub mod dirty_observer;
pub mod either;
pub mod map;
pub mod map_mut;
#[cfg(not(feature = "anchors_slotmap"))]
pub mod refmap;
pub mod then;
pub mod update_observer;

/// A trait automatically implemented for tuples of Anchors.
///
/// You'll likely want to `use` this trait in most of your programs, since it can create many
/// useful Anchors that derive their output incrementally from some other Anchors.
///
/// Methods here mirror the non-tuple implementations listed in [Anchor]; check that out if you're
/// curious what these methods do.
pub trait MultiAnchor<E: Engine>: Sized {
    type Target;

    fn map<F, Out>(self, f: F) -> Anchor<Out, E>
    where
        Out: 'static,
        F: 'static,
        map::Map<Self::Target, F, Out>: AnchorInner<E, Output = Out>;

    fn map_mut<F, Out>(self, initial: Out, f: F) -> Anchor<Out, E>
    where
        Out: 'static,
        F: 'static,
        map_mut::MapMut<Self::Target, F, Out>: AnchorInner<E, Output = Out>;

    fn then<F, Out>(self, f: F) -> Anchor<Out, E>
    where
        F: 'static,
        Out: 'static,
        then::Then<Self::Target, Out, F, E>: AnchorInner<E, Output = Out>;

    fn either<F, Out>(self, f: F) -> Anchor<Out, E>
    where
        F: 'static,
        Out: 'static,
        either::Either<Self::Target, Out, F, E>: AnchorInner<E, Output = Out>;

    fn cutoff<F, Out>(self, _f: F) -> Anchor<Out, E>
    where
        Out: 'static,
        F: 'static,
        cutoff::Cutoff<Self::Target, F>: AnchorInner<E, Output = Out>;

    #[cfg(not(feature = "anchors_slotmap"))]
    fn refmap<F, Out>(self, _f: F) -> Anchor<Out, E>
    where
        Out: 'static,
        F: 'static,
        refmap::RefMap<Self::Target, F>: AnchorInner<E, Output = Out>;

    /// 创建一个 DirtyObserverAnchor，用于观察输入 Anchor 的“脏传播”。
    ///
    /// 返回值为单调递增版本号 Anchor：只要任一输入被标脏（dirty），版本号 +1。
    /// 该信号允许假阳性（例如输入最终计算为 Unchanged 也会推进版本），用于做重活 gating。
    fn dirty_observer(self) -> Anchor<u64, E>
    where
        dirty_observer::DirtyObserverAnchor<Self::Target>: AnchorInner<E, Output = u64>;

    /// 创建一个 UpdateObserverAnchor，用于观察输入 Anchor 是否发生更新。
    ///
    /// 返回值为单调递增版本号 Anchor：只要任一输入 Updated，版本号 +1。
    /// 业务侧可以通过比较版本号是否变化来做“重活 gating”。
    fn update_observer(self) -> Anchor<u64, E>
    where
        update_observer::UpdateObserverAnchor<Self::Target>: AnchorInner<E, Output = u64>;
}

impl<O1, E> Anchor<O1, E>
where
    O1: 'static,
    E: Engine,
{
    /// Creates an Anchor that maps a number of incremental input values to some output value.
    /// The function `f` accepts inputs as references, and must return an owned value.
    /// `f` will always be recalled any time any input value changes.
    ///
    /// This method is mirrored by [MultiAnchor::map].
    ///
    /// ```
    /// use anchors::singlethread::*;
    /// let mut engine = Engine::new();
    /// let a = Anchor::constant(1);
    /// let b = Anchor::constant(2);
    ///
    /// // add the two numbers together; types have been added for clarity but are optional:
    /// let res: Anchor<usize> = (&a, &b).map(|a_val: &usize, b_val: &usize| -> usize {
    ///    *a_val+*b_val
    /// });
    ///
    /// assert_eq!(3, engine.get(&res));
    /// ```
    #[track_caller]
    pub fn map<F, Out>(&self, f: F) -> Anchor<Out, E>
    where
        Out: 'static,
        F: 'static,
        map::Map<(Anchor<O1, E>,), F, Out>: AnchorInner<E, Output = Out>,
    {
        E::mount(map::Map {
            anchors: (self.clone(),),
            f,
            output: None,
            output_stale: true,
            degraded_on_invalid: false,
            location: Location::caller(),
        })
    }

    #[track_caller]
    pub fn map_mut<F, Out>(&self, initial: Out, f: F) -> Anchor<Out, E>
    where
        Out: 'static,
        F: 'static,
        map_mut::MapMut<(Anchor<O1, E>,), F, Out>: AnchorInner<E, Output = Out>,
    {
        E::mount(map_mut::MapMut {
            anchors: (self.clone(),),
            f,
            output: initial,
            output_stale: true,
            degraded_on_invalid: false,
            location: Location::caller(),
        })
    }

    /// Creates an Anchor that maps a number of incremental input values to some output Anchor.
    /// With `then`, your computation graph can dynamically select an Anchor to recalculate based
    /// on some other incremental computation.
    /// The function `f` accepts inputs as references, and must return an owned `Anchor`.
    /// `f` will always be recalled any time any input value changes.
    ///
    /// This method is mirrored by [MultiAnchor::then].
    ///
    /// ```
    /// use anchors::singlethread::*;
    /// let mut engine = Engine::new();
    /// let decision = Anchor::constant(true);
    /// let num = Anchor::constant(1);
    ///
    /// // because of how we're using the `then` below, only one of these two
    /// // additions will actually be run
    /// let a = num.map(|num| *num + 1);
    /// let b = num.map(|num| *num + 2);
    ///
    /// // types have been added for clarity but are optional:
    /// let res: Anchor<usize> = decision.then(move |decision: &bool| {
    ///     if *decision {
    ///         a.clone()
    ///     } else {
    ///         b.clone()
    ///     }
    /// });
    ///
    /// assert_eq!(2, engine.get(&res));
    /// ```
    #[track_caller]
    pub fn then<F, Out>(&self, f: F) -> Anchor<Out, E>
    where
        F: 'static,
        Out: 'static,
        then::Then<(Anchor<O1, E>,), Out, F, E>: AnchorInner<E, Output = Out>,
    {
        E::mount(then::Then {
            anchors: (self.clone(),),
            f,
            f_anchor: None,
            location: Location::caller(),
            output_stale: true,
        })
    }

    /// `then` 的“owned 输出缓存”版本：要求 `Out: Clone`，把输出缓存到自身，避免 output 阶段再去 `ctx.get(f_anchor)`。
    ///
    /// 背景：slotmap 模式下拆树/GC 期可能出现 `PendingInvalidToken`，引擎会尝试“保留旧输出”降级。
    /// 但普通 `then` 的 `output()` 通过借用输出 Anchor 实现，仍可能在 token 失效时触发 `EngineContext::get` 硬崩溃。
    ///
    /// 该版本通过缓存 `Out`（clone）实现：
    /// - 正常情况下：当输出 Anchor Updated 时刷新缓存；
    /// - 遇到 `PendingInvalidToken`：冻结并保留缓存，返回 `Unchanged`，避免崩溃与重复 request 刷屏。
    #[track_caller]
    pub fn then_clone<F, Out>(&self, initial: Out, f: F) -> Anchor<Out, E>
    where
        Out: 'static + Clone,
        F: 'static,
        then::ThenClone1<O1, Out, F, E>: AnchorInner<E, Output = Out>,
    {
        E::mount(then::ThenClone1 {
            anchor: self.clone(),
            f,
            f_anchor: None,
            cached_output: initial,
            degraded_on_invalid: false,
            output_stale: true,
            location: Location::caller(),
        })
    }

    /// 与 `then` 类似，但要求输入实现 `Clone + PartialEq`，当本轮输入与上一轮等价时复用旧输出 Anchor，
    /// 避免频繁切换 token 导致的高度回填和重算。
    #[track_caller]
    pub fn then_dedupe<F, Out>(&self, f: F) -> Anchor<Out, E>
    where
        Out: 'static,
        F: 'static,
        O1: Clone + PartialEq + 'static,
        then::ThenDedupe1<O1, Out, F, E>: AnchorInner<E, Output = Out>,
    {
        E::mount(then::ThenDedupe1 {
            anchor: self.clone(),
            f,
            f_anchor: None,
            location: Location::caller(),
            output_stale: true,
            cached_input: None,
        })
    }

    /// 两入参去重版：输入实现 Clone + PartialEq 时，等价输入复用旧输出 Anchor。
    #[track_caller]
    pub fn then_dedupe2<O2, F, Out>(&self, other: &Anchor<O2, E>, f: F) -> Anchor<Out, E>
    where
        Out: 'static,
        F: 'static,
        O1: Clone + PartialEq + 'static,
        O2: Clone + PartialEq + 'static,
        then::ThenDedupe<(Anchor<O1, E>, Anchor<O2, E>), Out, F, E>: AnchorInner<E, Output = Out>,
    {
        E::mount(then::ThenDedupe {
            anchors: (self.clone(), other.clone()),
            f,
            f_anchor: None,
            cached_inputs: None,
            location: Location::caller(),
            output_stale: true,
        })
    }

    /// 三入参去重版。
    #[track_caller]
    pub fn then_dedupe3<O2, O3, F, Out>(
        &self,
        o2: &Anchor<O2, E>,
        o3: &Anchor<O3, E>,
        f: F,
    ) -> Anchor<Out, E>
    where
        Out: 'static,
        F: 'static,
        O1: Clone + PartialEq + 'static,
        O2: Clone + PartialEq + 'static,
        O3: Clone + PartialEq + 'static,
        then::ThenDedupe<(Anchor<O1, E>, Anchor<O2, E>, Anchor<O3, E>), Out, F, E>:
            AnchorInner<E, Output = Out>,
    {
        E::mount(then::ThenDedupe {
            anchors: (self.clone(), o2.clone(), o3.clone()),
            f,
            f_anchor: None,
            cached_inputs: None,
            location: Location::caller(),
            output_stale: true,
        })
    }

    #[track_caller]
    pub fn either<F, Out>(&self, f: F) -> Anchor<Out, E>
    where
        F: 'static,
        Out: 'static,
        either::Either<(Anchor<O1, E>,), Out, F, E>: AnchorInner<E, Output = Out>,
    {
        E::mount(either::Either {
            anchors: (self.clone(),),
            f,
            either_anchor: None,
            location: Location::caller(),
            output_stale: true,
            degraded_on_invalid: false,
        })
    }

    /// Creates an Anchor that maps some input reference to some output reference.
    /// Performance is critical here: `f` will always be recalled any time any downstream node
    /// requests the value of this Anchor, *not* just when an input value changes.
    /// It's also critical to note that due to constraints
    /// with Rust's lifetime system, these output references can not be owned values, and must
    /// live exactly as long as the input reference.
    ///
    /// This method is mirrored by [MultiAnchor::refmap].
    ///
    /// ```
    /// use anchors::singlethread::*;
    /// struct CantClone {val: usize};
    /// let mut engine = Engine::new();
    /// let tuple = Anchor::constant((CantClone{val: 1}, CantClone{val: 2}));
    ///
    /// // lookup the first value inside the tuple; types have been added for clarity but are optional:
    /// let res: Anchor<CantClone> = tuple.refmap(|tuple: &(CantClone, CantClone)| -> &CantClone {
    ///    &tuple.0
    /// });
    ///
    /// // check if the cantclone value is correct:
    /// let is_one = res.map(|tuple: &CantClone| -> bool {
    ///    tuple.val == 1
    /// });
    ///
    /// assert_eq!(true, engine.get(&is_one));
    /// ```
    #[track_caller]
    #[cfg(not(feature = "anchors_slotmap"))]
    pub fn refmap<F, Out>(&self, f: F) -> Anchor<Out, E>
    where
        Out: 'static,
        F: 'static,
        refmap::RefMap<(Anchor<O1, E>,), F>: AnchorInner<E, Output = Out>,
    {
        E::mount(refmap::RefMap {
            anchors: (self.clone(),),
            f,
            location: Location::caller(),
        })
    }

    /// Creates an Anchor that outputs its input. However, even if a value changes
    /// you may not want to recompute downstream nodes unless the value changes substantially.
    /// The function `f` accepts inputs as references, and must return true if Anchors that derive
    /// values from this cutoff should recalculate, or false if derivative Anchors should not recalculate.
    /// If this is the first calculation, `f` will be called, but return values of `false` will be ignored.
    /// `f` will always be recalled any time the input value changes.
    ///
    /// This method is mirrored by [MultiAnchor::cutoff].
    ///
    /// ```
    /// use anchors::singlethread::*;
    /// let mut engine = Engine::new();
    /// let num = Var::new(1i32);
    /// let cutoff = {
    ///     let mut old_num_opt: Option<i32> = None;
    ///     num.watch().cutoff(move |num| {
    ///         if let Some(old_num) = old_num_opt {
    ///             if (old_num - *num).abs() < 10 {
    ///                 return false;
    ///             }
    ///         }
    ///         old_num_opt = Some(*num);
    ///         true
    ///     })
    /// };
    /// let res = cutoff.map(|cutoff| *cutoff + 1);
    ///
    /// assert_eq!(2, engine.get(&res));
    ///
    /// // small changes don't cause recalculations
    /// num.set(5);
    /// assert_eq!(2, engine.get(&res));
    ///
    /// // but big changes do
    /// num.set(11);
    /// assert_eq!(12, engine.get(&res));
    /// ```
    #[track_caller]
    pub fn cutoff<F, Out>(&self, f: F) -> Anchor<Out, E>
    where
        Out: 'static,
        F: 'static,
        cutoff::Cutoff<(Anchor<O1, E>,), F>: AnchorInner<E, Output = Out>,
    {
        E::mount(cutoff::Cutoff {
            anchors: (self.clone(),),
            f,
            location: Location::caller(),
        })
    }

    /// 单 Anchor 版本的更新观察器。
    ///
    /// 典型用途：把某个关键 Anchor 的更新压缩成版本号，
    /// 下游按需决定是否重建 Snapshot 或刷新命中数据。
    #[track_caller]
    pub fn update_observer(&self) -> Anchor<u64, E> {
        E::mount(update_observer::UpdateObserverAnchor {
            anchors: (self.clone(),),
            version: 0,
            output_stale: true,
            degraded_on_invalid: false,
            location: Location::caller(),
        })
    }

    /// 单 Anchor 版本的脏传播观察器。
    ///
    /// 语义：只要该 Anchor 在依赖图里被标脏（dirty），版本号就会推进；
    /// 不保证该 Anchor 的输出值真的变化。
    #[track_caller]
    pub fn dirty_observer(&self) -> Anchor<u64, E> {
        E::mount(dirty_observer::DirtyObserverAnchor {
            anchors: (self.clone(),),
            version: 0,
            output_stale: true,
            degraded_on_invalid: false,
            location: Location::caller(),
        })
    }
    #[track_caller]
    pub fn debounce(&self) -> Anchor<O1, E>
    where
        O1: PartialEq + Copy,
    {
        let mut old_v: Option<O1> = None;
        self.cutoff(move |x| {
            if let Some(ov) = &old_v
                && ov == x
            {
                return false;
            }
            old_v = Some(*x);
            true
        })
    }
    #[track_caller]
    pub fn debounce_clone(&self) -> Anchor<O1, E>
    where
        O1: PartialEq + Clone,
    {
        let mut old_v: Option<O1> = None;
        self.cutoff(move |x| {
            if let Some(ov) = &old_v
                && ov == x
            {
                return false;
            }
            old_v = Some(x.clone());
            true
        })
    }
}

macro_rules! impl_tuple_ext {
    ($([$output_type:ident, $num:tt])+) => {
        impl <$($output_type,)+ E> Anchor<($($output_type,)+), E>
        where
            $(
                $output_type: Clone + PartialEq + 'static,
            )+
            E: Engine,
        {
            #[track_caller]
            pub fn split(&self) -> ($(Anchor<$output_type, E>,)+) {
                ////////////////////////////////////////////////////////////////////////////////
                // NOTE:
                // - split 原实现用 refmap 做字段投影（输出借用自输入），减少一次 owned 缓存；
                // - 但在动态子树/GC 场景里，输入 token 可能短暂失效：
                //   refmap 会在 output() 内部调用 ctx.get()，从而触发 EngineContext::get 的 panic；
                // - 这里改为 map + clone，把字段值缓存为 owned 输出，使 PendingInvalidToken 可安全降级。
                ////////////////////////////////////////////////////////////////////////////////
                ($(
                    self.map(|v| v.$num.clone()),
                )+)
            }
        }

        impl<$($output_type,)+ E> MultiAnchor<E> for ($(&Anchor<$output_type, E>,)+)
        where
            $(
                $output_type: 'static,
            )+
            E: Engine,
        {
            type Target = ($(Anchor<$output_type, E>,)+);

            #[track_caller]
            fn map<F, Out>(self, f: F) -> Anchor<Out, E>
            where
                Out: 'static,
                F: 'static,
                map::Map<Self::Target, F, Out>: AnchorInner<E, Output=Out>,
            {
                E::mount(map::Map {
                    anchors: ($(self.$num.clone(),)+),
                    f,
                    output: None,
                    output_stale: true,
                    degraded_on_invalid: false,
                    location: Location::caller(),
                })
            }

            #[track_caller]
            fn map_mut<F, Out>(self, initial: Out, f: F) -> Anchor<Out, E>
            where
                Out: 'static,
                F: 'static,
                map_mut::MapMut<Self::Target, F, Out>: AnchorInner<E, Output=Out>,
            {
                E::mount(map_mut::MapMut {
                    anchors: ($(self.$num.clone(),)+),
                    f,
                    output: initial,
                    output_stale: true,
                    degraded_on_invalid: false,
                    location: Location::caller(),
                })
            }

            #[track_caller]
            fn then<F, Out>(self, f: F) -> Anchor<Out, E>
            where
                F: 'static,
                Out: 'static,
                then::Then<Self::Target, Out, F, E>: AnchorInner<E, Output=Out>,
            {
                E::mount(then::Then {
                    anchors: ($(self.$num.clone(),)+),
                    f,
                    f_anchor: None,
                    location: Location::caller(),
                    output_stale: true,
                })
            }

            #[track_caller]
            fn either<F, Out>(self, f: F) -> Anchor<Out, E>
            where
                F:'static,
                Out: 'static,
                either::Either<Self::Target, Out, F, E>: AnchorInner<E, Output = Out>,
            {
                E::mount(either::Either {
                    anchors: ($(self.$num.clone(),)+),
                    f,
                    either_anchor: None,
                    location: Location::caller(),
                    output_stale: true,
                    degraded_on_invalid: false,
                })
            }


            #[track_caller]
            #[cfg(not(feature = "anchors_slotmap"))]
            fn refmap<F, Out>(self, f: F) -> Anchor<Out, E>
            where
                Out: 'static,
                F: 'static,
                refmap::RefMap<Self::Target, F>: AnchorInner<E, Output = Out>,
            {
                E::mount(refmap::RefMap {
                    anchors: ($(self.$num.clone(),)+),
                    f,
                    location: Location::caller(),
                })
            }

            #[track_caller]
            fn cutoff<F, Out>(self, f: F) -> Anchor<Out, E>
            where
                Out: 'static,
                F: 'static,
                cutoff::Cutoff<Self::Target, F>: AnchorInner<E, Output = Out>,
            {
                E::mount(cutoff::Cutoff {
                    anchors: ($(self.$num.clone(),)+),
                    f,
                    location: Location::caller(),
                })
            }

            #[track_caller]
            fn dirty_observer(self) -> Anchor<u64, E>
            where
                dirty_observer::DirtyObserverAnchor<Self::Target>:
                    AnchorInner<E, Output = u64>,
            {
                E::mount(dirty_observer::DirtyObserverAnchor {
                    anchors: ($(self.$num.clone(),)+),
                    version: 0,
                    output_stale: true,
                    degraded_on_invalid: false,
                    location: Location::caller(),
                })
            }

            #[track_caller]
            fn update_observer(self) -> Anchor<u64, E>
            where
                update_observer::UpdateObserverAnchor<Self::Target>:
                    AnchorInner<E, Output = u64>,
            {
                E::mount(update_observer::UpdateObserverAnchor {
                    anchors: ($(self.$num.clone(),)+),
                    version: 0,
                    output_stale: true,
                    degraded_on_invalid: false,
                    location: Location::caller(),
                })
            }
        }
    }
}

impl_tuple_ext! {
    [O0, 0]
}

impl_tuple_ext! {
    [O0, 0]
    [O1, 1]
}

impl_tuple_ext! {
    [O0, 0]
    [O1, 1]
    [O2, 2]
}

impl_tuple_ext! {
    [O0, 0]
    [O1, 1]
    [O2, 2]
    [O3, 3]
}

impl_tuple_ext! {
    [O0, 0]
    [O1, 1]
    [O2, 2]
    [O3, 3]
    [O4, 4]
}

impl_tuple_ext! {
    [O0, 0]
    [O1, 1]
    [O2, 2]
    [O3, 3]
    [O4, 4]
    [O5, 5]
}

impl_tuple_ext! {
    [O0, 0]
    [O1, 1]
    [O2, 2]
    [O3, 3]
    [O4, 4]
    [O5, 5]
    [O6, 6]
}

impl_tuple_ext! {
    [O0, 0]
    [O1, 1]
    [O2, 2]
    [O3, 3]
    [O4, 4]
    [O5, 5]
    [O6, 6]
    [O7, 7]
}

impl_tuple_ext! {
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
