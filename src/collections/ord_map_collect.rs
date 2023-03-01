/*
 * @Author: Rais
 * @Date: 2022-09-14 11:08:53
 * @LastEditTime: 2023-03-01 21:08:25
 * @LastEditors: Rais
 * @Description:
 */

use crate::im::OrdMap;

use crate::expert::{
    Anchor, AnchorHandle, AnchorInner, Engine, OutputContext, Poll, UpdateContext,
};
use std::panic::Location;

impl<I, V, E> From<Anchor<OrdMap<I, Anchor<V, E>>, E>> for Anchor<OrdMap<I, V>, E>
where
    <E as Engine>::AnchorHandle: PartialOrd + Ord,
    V: std::clone::Clone + 'static,
    I: 'static + Clone + std::cmp::Ord,
    E: Engine,
    // OrdMap<I, V>: std::cmp::Eq,
{
    fn from(value: Anchor<OrdMap<I, Anchor<V, E>>, E>) -> Self {
        value.then(|v| OrdMapCollect::new_to_anchor(v.clone()))
    }
}

impl<I, V, E> std::iter::FromIterator<(I, Anchor<V, E>)> for Anchor<OrdMap<I, V>, E>
where
    <E as Engine>::AnchorHandle: PartialOrd + Ord,
    V: std::clone::Clone + 'static,
    I: 'static + Clone + std::cmp::Ord,
    E: Engine,
    // OrdMap<I, V>: std::cmp::Eq,
{
    fn from_iter<T>(iter: T) -> Self
    where
        T: IntoIterator<Item = (I, Anchor<V, E>)>,
    {
        OrdMapCollect::new_to_anchor(iter.into_iter().collect())
    }
}

impl<'a, I, V, E> std::iter::FromIterator<&'a (I, Anchor<V, E>)> for Anchor<OrdMap<I, V>, E>
where
    <E as Engine>::AnchorHandle: PartialOrd + Ord,
    V: std::clone::Clone + 'static,
    I: 'static + Clone + std::cmp::Ord,
    E: Engine,
    // OrdMap<I, V>: std::cmp::Eq,
{
    fn from_iter<T>(iter: T) -> Self
    where
        T: IntoIterator<Item = &'a (I, Anchor<V, E>)>,
    {
        OrdMapCollect::new_to_anchor(iter.into_iter().cloned().collect())
    }
}

impl<'a, I, V, E> std::iter::FromIterator<(&'a I, &'a Anchor<V, E>)> for Anchor<OrdMap<I, V>, E>
where
    <E as Engine>::AnchorHandle: PartialOrd + Ord,
    V: std::clone::Clone + 'static,
    I: 'static + Clone + std::cmp::Ord,
    E: Engine,
    // OrdMap<I, V>: std::cmp::Eq,
{
    fn from_iter<T>(iter: T) -> Self
    where
        T: IntoIterator<Item = (&'a I, &'a Anchor<V, E>)>,
    {
        OrdMapCollect::new_to_anchor(
            iter.into_iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        )
    }
}

pub struct OrdMapCollect<I, V, E: Engine> {
    anchors: OrdMap<I, Anchor<V, E>>,
    vals: Option<OrdMap<I, V>>,
    dirty: bool,
    location: &'static Location<'static>,
}

impl<I: 'static + Clone + std::cmp::Ord, V, E: Engine> OrdMapCollect<I, V, E>
where
    <E as Engine>::AnchorHandle: PartialOrd + Ord,
    V: std::clone::Clone + 'static,
    //TODO 通过特化 制作PartialEq版本, (im_rc 在 PartialEq 的情况下 对比 ,没有Eq 性能高)
    //TODO 制作 vector smallvec 的对比版本
    // OrdMap<I, V>: std::cmp::Eq,
{
    #[track_caller]
    pub fn new_to_anchor(anchors: OrdMap<I, Anchor<V, E>>) -> Anchor<OrdMap<I, V>, E> {
        E::mount(Self {
            anchors,
            vals: None,
            dirty: true,
            location: Location::caller(),
        })
    }
}

impl<I, V, E> AnchorInner<E> for OrdMapCollect<I, V, E>
where
    <E as Engine>::AnchorHandle: PartialOrd + Ord,
    V: std::clone::Clone + 'static,
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
                .try_fold(vec![], |mut acc, (i, anchor)| {
                    let s = ctx.request(anchor, true);
                    if s != Poll::Pending {
                        acc.push((s, (i, anchor)));
                        Some(acc)
                    } else {
                        None
                    }
                });

            if polls.is_none() {
                return Poll::Pending;
            }

            self.dirty = false;

            if let Some(ref mut old_vals) = self.vals {
                for (poll, (i, anchor)) in polls.unwrap().iter() {
                    if &Poll::Updated == poll {
                        old_vals.insert((**i).clone(), ctx.get(anchor).clone());
                        changed = true;
                    }
                }
            } else {
                self.vals = Some(
                    self.anchors
                        .iter()
                        .map(|(i, anchor)| (i.clone(), ctx.get(anchor).clone()))
                        .collect(),
                );
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

    use crate::{collections::ord_map_collect::OrdMapCollect, im::OrdMap};

    use crate::{dict, singlethread::*};

    #[test]
    fn collect_k_change() {
        let mut engine = Engine::new();
        let a = Var::new(1);
        let b = Var::new(2);
        let c = Var::new(5);
        let d = Var::new(10);
        let dict = dict!(1usize=>a.watch(),2usize=>b.watch(),3usize=>c.watch());
        let f = Var::new(dict.clone());

        let nums = f.watch().then(|d| {
            let nums: Anchor<OrdMap<_, _>> = d.into_iter().collect();
            nums
        });
        let sum: Anchor<usize> = nums.map(|nums| nums.values().sum());
        assert_eq!(engine.get(&sum), 8);
        f.set(dict!(9usize=>a.watch(),2usize=>b.watch(),3usize=>c.watch()));
        assert_eq!(engine.get(&sum), 8);
        f.set(dict!(10usize=>d.watch(),2usize=>b.watch(),3usize=>c.watch()));
        assert_eq!(engine.get(&sum), 17);

        // ─────────────────────────────────────────────────────────────────────────────
        let f2 = Var::new(dict.clone());

        let nums2 = f2.watch().then(|d| {
            let nums2: Anchor<OrdMap<_, _>> = OrdMapCollect::new_to_anchor(d.clone());
            nums2
        });
        let sum: Anchor<usize> = nums2.map(|nums| nums.values().sum());
        assert_eq!(engine.get(&sum), 8);
        f2.set(dict!(9usize=>a.watch(),2usize=>b.watch(),3usize=>c.watch()));
        assert_eq!(engine.get(&sum), 8);
        f2.set(dict!(10usize=>d.watch(),2usize=>b.watch(),3usize=>c.watch()));
        assert_eq!(engine.get(&sum), 17);
        // ─────────────────────────────────────────────────────────────

        let f2 = Var::new(dict);

        let watch = f2.watch();
        let nums2: Anchor<OrdMap<_, _>> = watch.into();
        let sum: Anchor<usize> = nums2.map(|nums| nums.values().sum());
        assert_eq!(engine.get(&sum), 8);
        f2.set(dict!(9usize=>a.watch(),2usize=>b.watch(),3usize=>c.watch()));
        assert_eq!(engine.get(&sum), 8);
        f2.set(dict!(10usize=>d.watch(),2usize=>b.watch(),3usize=>c.watch()));
        assert_eq!(engine.get(&sum), 17);
    }

    #[test]
    fn collect() {
        let mut engine = Engine::new();
        let a = Var::new(1);
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

        let _bw = bcut.map(|v| {
            println!("b change");
            *v
        });
        let f = dict!(1usize=>a.watch(),2usize=>b.watch(),3usize=>c.watch());
        let nums: Anchor<OrdMap<_, _>> = f.into_iter().collect();
        let sum: Anchor<usize> = nums.map(|nums| nums.values().sum());
        let ns: Anchor<usize> = nums.map(|nums: &OrdMap<_, _>| nums.len());

        assert_eq!(engine.get(&sum), 8);

        a.set(2);
        assert_eq!(engine.get(&sum), 9);

        c.set(1);
        assert_eq!(engine.get(&sum), 5);
        println!("ns {}", engine.get(&ns));
        b.set(9);
        println!("after b set: {}", engine.get(&sum)); // [2,1,9]
        assert_eq!(engine.get(&sum), 12);

        b.set(9);
        println!("after b set2: {}", engine.get(&sum));
        assert_eq!(engine.get(&sum), 12);
    }
}
