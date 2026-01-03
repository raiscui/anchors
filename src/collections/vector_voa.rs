/*
 * @Author: Rais
 * @Date: 2023-04-04 23:56:14
 * @LastEditTime: 2025-12-09 01:29:54
 * @LastEditors: Rais
 * @Description:
 */
use crate::im_rc::Vector;
use crate::im_rc::vector;

use crate::expert::ValOrAnchor;

use crate::expert::{
    Anchor, AnchorHandle, AnchorInner, Engine, OutputContext, Poll, UpdateContext,
};
use std::panic::Location;

use super::ord_map_methods::Dict;

impl<I, V, E> From<Dict<I, ValOrAnchor<V, E>>> for Anchor<Vector<V>, E>
where
    <E as Engine>::AnchorHandle: PartialOrd + Ord,
    V: std::clone::Clone + 'static,
    I: 'static + Clone + std::cmp::Ord,
    E: Engine,
    // OrdMap<I, V>: std::cmp::Eq,
{
    #[track_caller]
    fn from(value: Dict<I, ValOrAnchor<V, E>>) -> Self {
        VectorVOACollect::new_to_anchor(value.values().cloned().collect())
    }
}
impl<I, V, E> From<Anchor<Dict<I, ValOrAnchor<V, E>>, E>> for Anchor<Vector<V>, E>
where
    <E as Engine>::AnchorHandle: PartialOrd + Ord,
    V: std::clone::Clone + 'static,
    I: 'static + Clone + std::cmp::Ord,
    E: Engine,
    // OrdMap<I, V>: std::cmp::Eq,
{
    #[track_caller]
    fn from(value: Anchor<Dict<I, ValOrAnchor<V, E>>, E>) -> Self {
        value.then(|v| VectorVOACollect::new_to_anchor(v.values().cloned().collect()))
    }
}

impl<V, E> From<Vector<ValOrAnchor<V, E>>> for Anchor<Vector<V>, E>
where
    <E as Engine>::AnchorHandle: PartialOrd + Ord,
    V: std::clone::Clone + 'static,
    E: Engine,
    // OrdMap<I, V>: std::cmp::Eq,
{
    #[track_caller]
    fn from(value: Vector<ValOrAnchor<V, E>>) -> Self {
        VectorVOACollect::new_to_anchor(value)
    }
}
impl<V, E> From<&Anchor<Vector<ValOrAnchor<V, E>>, E>> for Anchor<Vector<V>, E>
where
    <E as Engine>::AnchorHandle: PartialOrd + Ord,
    V: std::clone::Clone + 'static,
    E: Engine,
    // OrdMap<I, V>: std::cmp::Eq,
{
    #[track_caller]
    fn from(value: &Anchor<Vector<ValOrAnchor<V, E>>, E>) -> Self {
        value.then(|v| VectorVOACollect::new_to_anchor(v.clone()))
    }
}
impl<V, E> From<Anchor<Vector<ValOrAnchor<V, E>>, E>> for Anchor<Vector<V>, E>
where
    <E as Engine>::AnchorHandle: PartialOrd + Ord,
    V: std::clone::Clone + 'static,
    E: Engine,
    // OrdMap<I, V>: std::cmp::Eq,
{
    #[track_caller]
    fn from(value: Anchor<Vector<ValOrAnchor<V, E>>, E>) -> Self {
        value.then(|v| VectorVOACollect::new_to_anchor(v.clone()))
    }
}

impl<I: 'static + Clone, E: Engine> std::iter::FromIterator<ValOrAnchor<I, E>>
    for Anchor<Vector<I>, E>
{
    #[track_caller]
    fn from_iter<T>(iter: T) -> Self
    where
        T: IntoIterator<Item = ValOrAnchor<I, E>>,
    {
        VectorVOACollect::new_to_anchor(iter.into_iter().collect())
    }
}

impl<'a, I: 'static + Clone, E: Engine> std::iter::FromIterator<&'a ValOrAnchor<I, E>>
    for Anchor<Vector<I>, E>
{
    #[track_caller]
    fn from_iter<T>(iter: T) -> Self
    where
        T: IntoIterator<Item = &'a ValOrAnchor<I, E>>,
    {
        VectorVOACollect::new_to_anchor(iter.into_iter().cloned().collect())
    }
}

pub struct VectorVOACollect<T, E: Engine> {
    anchors: Vector<ValOrAnchor<T, E>>,
    vals: Option<Vector<T>>,
    location: &'static Location<'static>,
    dirty: bool,
}

impl<T: 'static + Clone, E: Engine> VectorVOACollect<T, E> {
    #[track_caller]
    pub fn new_to_anchor(anchors: Vector<ValOrAnchor<T, E>>) -> Anchor<Vector<T>, E> {
        E::mount(Self {
            anchors,
            vals: None,
            location: Location::caller(),
            dirty: true,
        })
    }
}

impl<T: 'static + Clone, E: Engine> AnchorInner<E> for VectorVOACollect<T, E> {
    type Output = Vector<T>;
    fn dirty(&mut self, _edge: &<E::AnchorHandle as AnchorHandle>::Token) {
        // self.vals = None;
        self.dirty = true;
    }

    fn poll_updated<G: UpdateContext<Engine = E>>(&mut self, ctx: &mut G) -> Poll {
        let mut changed = false;
        if self.dirty {
            ////////////////////////////////////////////////////////////////////////////////
            // 关键语义：
            // - `PendingInvalidToken` 不是“还没 ready”，而是“永远不会再 ready”。
            // - 若仍把它当成 Pending，会导致每轮 stabilize 都重复 request 死 token，
            //   进而刷 `[anchors][invalid_token][request]` 的 1/10/100 采样日志，并且图永远不收敛。
            //
            // 这里的策略：把该元素视为“已被移除”，从本收集器的 anchors 列表里剔除；
            // 然后基于剩余元素重建输出，保证收敛并停止对死 token 的重复 request。
            ////////////////////////////////////////////////////////////////////////////////
            let mut invalid_mask = vec![false; self.anchors.len()];
            let mut polls: Vec<(Poll, Option<&Anchor<T, E>>)> =
                Vec::with_capacity(self.anchors.len());

            for (idx, voa) in self.anchors.iter().enumerate() {
                match voa {
                    ValOrAnchor::Val(_) => {
                        polls.push((Poll::Unchanged, None));
                    }
                    ValOrAnchor::Anchor(an) => {
                        let s = ctx.request(an, true);
                        match s {
                            Poll::Pending | Poll::PendingDefer => return Poll::Pending,
                            Poll::PendingInvalidToken => {
                                invalid_mask[idx] = true;
                                polls.push((s, Some(an)));
                            }
                            _ => {
                                polls.push((s, Some(an)));
                            }
                        }
                    }
                }
            }

            self.dirty = false;

            if invalid_mask.iter().any(|v| *v) {
                let invalid_count = invalid_mask.iter().filter(|v| **v).count();

                // ──────────────────────────────────────────────────────────────
                // 剔除已失效 token 对应的元素（仅移除 Anchor 项，Val 项保持）。
                // ──────────────────────────────────────────────────────────────
                let pool =
                    vector::RRBPool::<ValOrAnchor<T, E>>::new(self.anchors.len() - invalid_count);
                let mut next_anchors = Vector::with_pool(&pool);
                self.anchors
                    .iter()
                    .enumerate()
                    .filter_map(|(idx, voa)| {
                        if invalid_mask[idx] {
                            None
                        } else {
                            Some(voa.clone())
                        }
                    })
                    .collect_into(&mut next_anchors);

                // ──────────────────────────────────────────────────────────────
                // 失效 token 已被移除，此处可安全重建输出（剩余 Anchor 均已 request 且 ready）。
                // ──────────────────────────────────────────────────────────────
                let pool = vector::RRBPool::<T>::new(next_anchors.len());
                let mut vals = Vector::with_pool(&pool);
                next_anchors
                    .iter()
                    .map(|voa| match voa {
                        ValOrAnchor::Val(v) => v.clone(),
                        ValOrAnchor::Anchor(an) => ctx.get(an).clone(),
                    })
                    .collect_into(&mut vals);

                self.anchors = next_anchors;
                self.vals = Some(vals);
                return Poll::Updated;
            }

            // ─────────────────────────────────────────────────────────────────────────────
            // self.anchors.iter().for_each(|a| {
            //     let s = ctx.request(a, true);
            //     println!("{s:?}")
            // });
            // ─────────────────────────────────────────────────────

            if let Some(ref mut old_vals) = self.vals {
                for (old_val, (poll, opt_voa)) in old_vals.iter_mut().zip(polls.iter()) {
                    match opt_voa {
                        Some(voa) if &Poll::Updated == poll => {
                            *old_val = ctx.get(voa).clone();
                            changed = true;
                        }
                        _ => {}
                    }
                }
            } else {
                let pool = vector::RRBPool::<T>::new(self.anchors.len());
                let mut vals = Vector::with_pool(&pool);
                self.anchors
                    .iter()
                    .map(|voa| match voa {
                        ValOrAnchor::Val(v) => v.clone(),
                        ValOrAnchor::Anchor(an) => ctx.get(an).clone(),
                    })
                    .collect_into(&mut vals);

                self.vals = Some(vals);
                changed = true;

                // ─────────────────────────────────────────────

                // self.vals = Some(
                //     self.anchors
                //         .iter()
                //         .map(|anchor| ctx.get(anchor).clone())
                //         .collect(),
                // );
                // changed = true;
            }

            // ─────────────────────────────────────────────────────

            // self.vals = Some(
            //     self.anchors
            //         .iter()
            //         .map(|anchor| ctx.get(anchor).clone())
            //         .collect(),
            // );

            // changed = true;
        }

        if changed {
            Poll::Updated
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
        self.vals.as_ref().unwrap()
    }

    fn debug_location(&self) -> Option<(&'static str, &'static Location<'static>)> {
        Some(("VectorVOACollect", self.location))
    }
}

#[cfg(test)]
mod test {

    use crate::{
        expert::CastIntoValOrAnchor,
        im_rc::{Vector, vector},
    };

    use crate::expert::Engine as EngineTrait;
    use crate::singlethread::*;
    use crate::{expert::AnchorHandle, expert::DirtyHandle, expert::Poll, expert::UpdateContext};
    use std::any::Any;
    use std::collections::HashMap;
    use std::panic::Location;

    #[test]
    fn collect() {
        let mut engine = Engine::new();
        let a = Var::new(1usize);
        let b = Var::new(2);
        let c = Var::new(5);
        let bcut = {
            let mut old_num_opt: Option<usize> = None;
            b.watch().cutoff(move |num| {
                if let Some(old_num) = old_num_opt {
                    if old_num == *num {
                        return false;
                    }
                }
                old_num_opt = Some(*num);
                true
            })
        };

        let bw = bcut.map(|v| {
            println!("b change");
            *v
        });
        let nums: Anchor<Vector<usize>> = vector![
            a.watch().cast_into(),
            22.into(),
            bw.into(),
            c.watch().into()
        ]
        .into_iter()
        .collect();
        let sum: Anchor<usize> = nums.map(|nums| nums.iter().sum());
        let ns: Anchor<usize> = nums.map(|nums: &Vector<_>| nums.len());

        assert_eq!(engine.get(&sum), 8 + 22);

        a.set(2);
        assert_eq!(engine.get(&sum), 9 + 22);

        c.set(1);
        assert_eq!(engine.get(&sum), 2 + 2 + 1 + 22);
        println!("ns {}", engine.get(&ns));
        b.set(9);
        println!("after b set: {}", engine.get(&sum));
        b.set(9);
        println!("after b set2: {}", engine.get(&sum));
    }

    #[test]
    fn collect_should_drop_invalid_token_instead_of_pending_forever() {
        ////////////////////////////////////////////////////////////////////////////////
        // 该测试验证一个关键契约：
        // - 当某个元素 Anchor request 返回 `PendingInvalidToken` 时，收集器不能返回 Pending；
        // - 否则会导致每轮 stabilize 都重复 request 死 token，刷屏并且图永远不收敛；
        // - 正确行为是：把该元素当作“已移除”，从收集列表剔除，并重建输出。
        ////////////////////////////////////////////////////////////////////////////////

        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        struct DummyToken(u64);

        #[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        struct DummyAnchorHandle {
            token: DummyToken,
        }

        impl AnchorHandle for DummyAnchorHandle {
            type Token = DummyToken;

            fn token(&self) -> Self::Token {
                self.token
            }
        }

        #[derive(Clone)]
        struct DummyDirtyHandle;

        impl DirtyHandle for DummyDirtyHandle {
            fn mark_dirty(&self) {}
        }

        struct DummyEngine;

        impl EngineTrait for DummyEngine {
            type AnchorHandle = DummyAnchorHandle;
            type DirtyHandle = DummyDirtyHandle;

            fn mount<I: crate::expert::AnchorInner<Self> + 'static>(
                _inner: I,
            ) -> crate::expert::Anchor<I::Output, Self> {
                unreachable!("该测试直接构造 VectorVOACollect，不需要 mount")
            }
        }

        struct DummyCtx {
            polls: HashMap<DummyToken, Poll>,
            vals: HashMap<DummyToken, u32>,
        }

        impl DummyCtx {
            fn new() -> Self {
                Self {
                    polls: HashMap::new(),
                    vals: HashMap::new(),
                }
            }
        }

        impl UpdateContext for DummyCtx {
            type Engine = DummyEngine;

            fn get<'out, 'slf, O: 'static>(
                &'slf self,
                anchor: &crate::expert::Anchor<O, Self::Engine>,
            ) -> &'out O
            where
                'slf: 'out,
            {
                let token = anchor.token();
                let val = self.vals.get(&token).expect("缺少测试值");
                let any: &dyn Any = val;
                any.downcast_ref::<O>()
                    .expect("DummyCtx 只支持 O=u32 的 get")
            }

            fn request<O: 'static>(
                &mut self,
                anchor: &crate::expert::Anchor<O, Self::Engine>,
                _necessary: bool,
            ) -> Poll {
                match self.polls.get(&anchor.token()) {
                    Some(Poll::Updated) => Poll::Updated,
                    Some(Poll::Unchanged) => Poll::Unchanged,
                    Some(Poll::Pending) => Poll::Pending,
                    Some(Poll::PendingDefer) => Poll::PendingDefer,
                    Some(Poll::PendingInvalidToken) => Poll::PendingInvalidToken,
                    None => Poll::Unchanged,
                }
            }

            fn unrequest<O: 'static>(&mut self, _anchor: &crate::expert::Anchor<O, Self::Engine>) {}

            fn dirty_handle(&mut self) -> <Self::Engine as EngineTrait>::DirtyHandle {
                DummyDirtyHandle
            }
        }

        let a = crate::expert::Anchor::<u32, DummyEngine>::new_from_expert(DummyAnchorHandle {
            token: DummyToken(1),
        });
        let b = crate::expert::Anchor::<u32, DummyEngine>::new_from_expert(DummyAnchorHandle {
            token: DummyToken(2),
        });

        let mut inner = super::VectorVOACollect::<u32, DummyEngine> {
            anchors: vector![10u32.into(), a.into(), b.into()],
            vals: None,
            location: Location::caller(),
            dirty: true,
        };

        let mut ctx = DummyCtx::new();
        ctx.polls.insert(DummyToken(1), Poll::Unchanged);
        ctx.polls.insert(DummyToken(2), Poll::PendingInvalidToken);
        ctx.vals.insert(DummyToken(1), 5);

        // b 是 invalid token：应被移除，输出只剩 [10, 5]
        assert_eq!(
            crate::expert::AnchorInner::poll_updated(&mut inner, &mut ctx),
            Poll::Updated
        );
        assert_eq!(
            inner.vals.clone().expect("必须生成输出"),
            vector![10u32, 5u32]
        );

        // 后续再次 poll，不应陷入 Pending，也不应崩溃
        assert_eq!(
            crate::expert::AnchorInner::poll_updated(&mut inner, &mut ctx),
            Poll::Unchanged
        );
    }
}
