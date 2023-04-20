/*
 * @Author: Rais
 * @Date: 2023-04-04 23:56:14
 * @LastEditTime: 2023-04-18 12:22:14
 * @LastEditors: Rais
 * @Description:
 */
use im_rc::vector;

use crate::{expert::ValOrAnchor, im::Vector};

use crate::expert::{
    Anchor, AnchorHandle, AnchorInner, Engine, OutputContext, Poll, UpdateContext,
};
use std::{any::Any, panic::Location};

use super::ord_map_methods::Dict;

impl<I, V, E> From<Dict<I, ValOrAnchor<V, E>>> for Anchor<Vector<V>, E>
where
    <E as Engine>::AnchorHandle: PartialOrd + Ord,
    V: std::clone::Clone + 'static,
    I: 'static + Clone + std::cmp::Ord,
    E: Engine,
    // OrdMap<I, V>: std::cmp::Eq,
{
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
    fn from(value: Anchor<Vector<ValOrAnchor<V, E>>, E>) -> Self {
        value.then(|v| VectorVOACollect::new_to_anchor(v.clone()))
    }
}

impl<I: 'static + Clone, E: Engine> std::iter::FromIterator<ValOrAnchor<I, E>>
    for Anchor<Vector<I>, E>
{
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
            let polls = self
                .anchors
                .iter()
                .try_fold(vec![], |mut acc, voa| match voa {
                    ValOrAnchor::Val(_) => {
                        acc.push((Poll::Unchanged, None));
                        Some(acc)
                    }
                    ValOrAnchor::Anchor(an) => {
                        let s = ctx.request(an, true);
                        if s == Poll::Pending {
                            None
                        } else {
                            acc.push((s, Some(an)));
                            Some(acc)
                        }
                    }
                });

            if polls.is_none() {
                return Poll::Pending;
            }
            self.dirty = false;

            // ─────────────────────────────────────────────────────────────────────────────
            // self.anchors.iter().for_each(|a| {
            //     let s = ctx.request(a, true);
            //     println!("{s:?}")
            // });
            // ─────────────────────────────────────────────────────

            if let Some(ref mut old_vals) = self.vals {
                for (old_val, (poll, opt_voa)) in old_vals.iter_mut().zip(polls.unwrap().iter()) {
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
        im::{vector, Vector},
    };

    use crate::singlethread::*;

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
}
