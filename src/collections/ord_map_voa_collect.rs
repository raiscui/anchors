/*
 * @Author: Rais
 * @Date: 2023-04-03 14:30:01
 * @LastEditTime: 2023-04-05 00:12:41
 * @LastEditors: Rais
 * @Description:
 */

use im_rc::ordmap;

use crate::{expert::ValOrAnchor, im::OrdMap};

use crate::expert::{
    Anchor, AnchorHandle, AnchorInner, Engine, OutputContext, Poll, UpdateContext,
};
use std::panic::Location;
// ─────────────────────────────────────────────────────────────────────────────

impl<I, V, E> From<OrdMap<I, ValOrAnchor<V, E>>> for Anchor<OrdMap<I, V>, E>
where
    <E as Engine>::AnchorHandle: PartialOrd + Ord,
    V: std::clone::Clone + 'static + std::cmp::PartialEq,
    I: 'static + Clone + std::cmp::Ord,
    E: Engine,
    // OrdMap<I, V>: std::cmp::Eq,
{
    fn from(value: OrdMap<I, ValOrAnchor<V, E>>) -> Self {
        OrdMapVOACollect::new_to_anchor(value)
    }
}
impl<I, V, E> From<Anchor<OrdMap<I, ValOrAnchor<V, E>>, E>> for Anchor<OrdMap<I, V>, E>
where
    <E as Engine>::AnchorHandle: PartialOrd + Ord,
    V: std::clone::Clone + 'static + std::cmp::PartialEq,
    I: 'static + Clone + std::cmp::Ord,
    E: Engine,
    // OrdMap<I, V>: std::cmp::Eq,
{
    fn from(value: Anchor<OrdMap<I, ValOrAnchor<V, E>>, E>) -> Self {
        value.then(|v| OrdMapVOACollect::new_to_anchor(v.clone()))
    }
}

impl<I, V, E> std::iter::FromIterator<(I, ValOrAnchor<V, E>)> for Anchor<OrdMap<I, V>, E>
where
    <E as Engine>::AnchorHandle: PartialOrd + Ord,
    V: std::clone::Clone + 'static + std::cmp::PartialEq,
    I: 'static + Clone + std::cmp::Ord,
    E: Engine,
    // OrdMap<I, V>: std::cmp::Eq,
{
    fn from_iter<T>(iter: T) -> Self
    where
        T: IntoIterator<Item = (I, ValOrAnchor<V, E>)>,
    {
        OrdMapVOACollect::new_to_anchor(iter.into_iter().collect())
    }
}

impl<'a, I, V, E> std::iter::FromIterator<&'a (I, ValOrAnchor<V, E>)> for Anchor<OrdMap<I, V>, E>
where
    <E as Engine>::AnchorHandle: PartialOrd + Ord,
    V: std::clone::Clone + 'static + std::cmp::PartialEq,
    I: 'static + Clone + std::cmp::Ord,
    E: Engine,
    // OrdMap<I, V>: std::cmp::Eq,
{
    fn from_iter<T>(iter: T) -> Self
    where
        T: IntoIterator<Item = &'a (I, ValOrAnchor<V, E>)>,
    {
        OrdMapVOACollect::new_to_anchor(iter.into_iter().cloned().collect())
    }
}

impl<'a, I, V, E> std::iter::FromIterator<(&'a I, &'a ValOrAnchor<V, E>)>
    for Anchor<OrdMap<I, V>, E>
where
    <E as Engine>::AnchorHandle: PartialOrd + Ord,
    V: std::clone::Clone + 'static + std::cmp::PartialEq,
    I: 'static + Clone + std::cmp::Ord,
    E: Engine,
    // OrdMap<I, V>: std::cmp::Eq,
{
    fn from_iter<T>(iter: T) -> Self
    where
        T: IntoIterator<Item = (&'a I, &'a ValOrAnchor<V, E>)>,
    {
        OrdMapVOACollect::new_to_anchor(
            iter.into_iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        )
    }
}

pub struct OrdMapVOACollect<I, V, E: Engine> {
    anchors: OrdMap<I, ValOrAnchor<V, E>>,
    vals: Option<OrdMap<I, V>>,
    dirty: bool,
    location: &'static Location<'static>,
}

impl<I: 'static + Clone + std::cmp::Ord, V, E: Engine> OrdMapVOACollect<I, V, E>
where
    <E as Engine>::AnchorHandle: PartialOrd + Ord,
    V: std::clone::Clone + 'static + std::cmp::PartialEq,
    //TODO 通过特化 制作PartialEq版本, (im_rc 在 PartialEq 的情况下 对比 ,没有Eq 性能高)
    //TODO 制作 vector smallvec 的对比版本
    // OrdMap<I, V>: std::cmp::Eq,
{
    #[track_caller]
    pub fn new_to_anchor(anchors: OrdMap<I, ValOrAnchor<V, E>>) -> Anchor<OrdMap<I, V>, E> {
        E::mount(Self {
            anchors,
            vals: None,
            dirty: true,
            location: Location::caller(),
        })
    }
}

impl<I, V, E> AnchorInner<E> for OrdMapVOACollect<I, V, E>
where
    <E as Engine>::AnchorHandle: PartialOrd + Ord,
    V: std::clone::Clone + 'static + std::cmp::PartialEq,
    I: 'static + Clone + std::cmp::Ord,
    E: Engine,
    // OrdMap<I, V>: std::cmp::Eq,
{
    type Output = OrdMap<I, V>;
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
                .try_fold(vec![], |mut acc, (i, anchor)| match anchor {
                    ValOrAnchor::Val(_) => {
                        #[cfg(debug_assertions)]
                        acc.push((Poll::Unchanged, (i, anchor)));
                        #[cfg(not(debug_assertions))]
                        acc.push((Poll::Unchanged, (i, None)));

                        Some(acc)
                    }
                    ValOrAnchor::Anchor(an) => {
                        let s = ctx.request(an, true);
                        if s == Poll::Pending {
                            None
                        } else {
                            #[cfg(debug_assertions)]
                            acc.push((s, (i, anchor)));
                            #[cfg(not(debug_assertions))]
                            acc.push((s, (i, Some(an))));

                            Some(acc)
                        }
                    }
                });

            if polls.is_none() {
                return Poll::Pending;
            }

            self.dirty = false;

            if let Some(ref mut old_vals) = self.vals {
                for (poll, (i, opt_an)) in &polls.unwrap() {
                    #[cfg(debug_assertions)]
                    {
                        match opt_an {
                            ValOrAnchor::Val(v) => {
                                debug_assert!(old_vals.get(i).unwrap() == v);
                            }
                            ValOrAnchor::Anchor(an) => {
                                if &Poll::Updated == poll {
                                    old_vals.insert((**i).clone(), ctx.get(an).clone());
                                    changed = true;
                                }
                            }
                        }
                    }
                    #[cfg(not(debug_assertions))]
                    {
                        match opt_an {
                            Some(an) if &Poll::Updated == poll => {
                                old_vals.insert((**i).clone(), ctx.get(an).clone());
                                changed = true;
                            }
                            _ => {}
                        }
                    }
                }
            } else {
                // self.vals = Some(
                //     self.anchors
                //         .iter()
                //         .map(|(i, anchor)| (i.clone(), ctx.get(anchor).clone()))
                //         .collect(),
                // );
                // changed = true;

                let pool = ordmap::OrdMapPool::new(self.anchors.len());
                let mut dict = OrdMap::with_pool(&pool);

                self.anchors
                    .iter()
                    .map(|(i, voa)| match voa {
                        ValOrAnchor::Val(v) => (i.clone(), v.clone()),
                        ValOrAnchor::Anchor(an) => (i.clone(), ctx.get(an).clone()),
                    })
                    .collect_into(&mut dict);

                self.vals = Some(dict);
                changed = true;
            }
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
        Some(("DictCollect", self.location))
    }
}

#[cfg(test)]
mod test {

    use crate::{collections::ord_map_methods::Dict, im::OrdMap};

    use crate::{dict, singlethread::*};

    #[test]
    fn collect_k_change() {
        let mut engine = Engine::new();
        let a = Var::new(1);
        let av = 1;
        let b = Var::new(2);
        let bv = 2;
        let c = Var::new(5);
        let cv = 5;
        let d = Var::new(10);
        let dv = 10;

        // ─────────────────────────────────────────────────────────────

        let dict: Dict<usize, ValOrAnchor<usize>> = dict!(
            1usize=>a.watch().into(),
            2usize=>av.into(),
            3usize=>b.watch().into(),
            4usize=>bv.into(),
            5usize=>c.watch().into(),
            6usize=>cv.into(),
            7usize=>d.watch().into(),
            8usize=>dv.into()
        );
        let f = Var::new(dict.clone());

        let nums = f.watch().then(|dict| {
            // let nums: Anchor<OrdMap<_, _>> = d.into_iter().collect();
            let nums: Anchor<OrdMap<usize, usize>> = dict.clone().into();
            nums
        });
        let sum: Anchor<usize> = nums.map(|nums| nums.values().sum());
        assert_eq!(engine.get(&sum), 36);
        f.set(dict!(
            9usize=>a.watch().into(),
            2usize=>av.into(),
            3usize=>b.watch().into(),
            4usize=>bv.into(),
            5usize=>c.watch().into(),
            6usize=>cv.into(),
            7usize=>d.watch().into(),
            8usize=>dv.into()
        ));
        assert_eq!(engine.get(&sum), 36);
        f.set(dict!(

            1usize=>bv.into(),
            4usize=>c.watch().into(),


            7usize=>dv.into()
        ));
        assert_eq!(engine.get(&sum), 17);

        // ─────────────────────────────────────────────────────────────

        let f2 = Var::new(dict);

        let watch = f2.watch();
        let nums2: Anchor<OrdMap<usize, usize>> = watch.into();
        let sum2: Anchor<usize> = nums2.map(|nums| nums.values().sum());
        assert_eq!(engine.get(&sum2), 36);
        f2.set(dict!(
            9usize=>a.watch().into(),
            2usize=>av.into(),
            3usize=>b.watch().into(),
            4usize=>bv.into(),
            5usize=>c.watch().into(),
            6usize=>cv.into(),
            7usize=>d.watch().into(),
            8usize=>dv.into()
        ));
        assert_eq!(engine.get(&sum2), 36);
        f2.set(dict!(

            1usize=>bv.into(),
            4usize=>c.watch().into(),


            7usize=>dv.into()
        ));
        assert_eq!(engine.get(&sum2), 17);
    }

    #[test]
    fn collect() {
        let mut engine = Engine::new();

        let a = Var::new(1);
        let av = 1;
        let b = Var::new(2);
        let bv = 2;
        let c = Var::new(5);
        let cv = 5;
        let d = Var::new(10);
        let dv = 10;

        let dict: Dict<usize, ValOrAnchor<usize>> = dict!(
            1usize=>a.watch().into(),
            2usize=>av.into(),
            3usize=>b.watch().into(),
            4usize=>bv.into(),
            5usize=>c.watch().into(),
            6usize=>cv.into(),
            7usize=>d.watch().into(),
            8usize=>dv.into()
        );
        let f = Var::new(dict.clone());

        let nums: Anchor<OrdMap<usize, usize>> = f.watch().into();
        let sum: Anchor<usize> = nums.map(|nums| nums.values().sum());

        assert_eq!(engine.get(&sum), 36);

        a.set(2);
        assert_eq!(engine.get(&sum), 37);

        f.set(dict!(
            1usize=>a.watch().into(),
            2usize=>10.into(),
            3usize=>b.watch().into(),
            4usize=>bv.into(),
            5usize=>c.watch().into(),
            6usize=>cv.into(),
            7usize=>d.watch().into(),
            8usize=>dv.into()
        ));
        assert_eq!(engine.get(&sum), 37 + 9);
    }
}
