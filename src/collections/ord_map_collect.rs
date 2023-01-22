/*
 * @Author: Rais
 * @Date: 2022-09-14 11:08:53
 * @LastEditTime: 2023-01-19 16:19:47
 * @LastEditors: Rais
 * @Description:
 */
/*
 * @Author: Rais
 * @Date: 2022-07-11 22:12:01
 * @LastEditTime: 2022-07-11 22:59:29
 * @LastEditors: Rais
 * @Description:
 */

use im_rc::OrdMap;

use crate::expert::{
    Anchor, AnchorHandle, AnchorInner, Engine, OutputContext, Poll, UpdateContext,
};
use std::panic::Location;

impl<I, V, E> std::iter::FromIterator<(I, Anchor<V, E>)> for Anchor<OrdMap<I, V>, E>
where
    <E as Engine>::AnchorHandle: PartialOrd + Ord,
    V: std::clone::Clone + 'static,
    I: 'static + Clone + std::cmp::Ord,
    E: Engine,
    OrdMap<I, V>: std::cmp::Eq,
{
    fn from_iter<T>(iter: T) -> Self
    where
        T: IntoIterator<Item = (I, Anchor<V, E>)>,
    {
        OrdMapCollect::new(iter.into_iter().collect())
    }
}

impl<'a, I, V, E> std::iter::FromIterator<&'a (I, Anchor<V, E>)> for Anchor<OrdMap<I, V>, E>
where
    <E as Engine>::AnchorHandle: PartialOrd + Ord,
    V: std::clone::Clone + 'static,
    I: 'static + Clone + std::cmp::Ord,
    E: Engine,
    OrdMap<I, V>: std::cmp::Eq,
{
    fn from_iter<T>(iter: T) -> Self
    where
        T: IntoIterator<Item = &'a (I, Anchor<V, E>)>,
    {
        OrdMapCollect::new(iter.into_iter().cloned().collect())
    }
}

struct OrdMapCollect<I, V, E: Engine> {
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
    OrdMap<I, V>: std::cmp::Eq,
{
    #[track_caller]
    pub fn new(anchors: OrdMap<I, Anchor<V, E>>) -> Anchor<OrdMap<I, V>, E> {
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
    OrdMap<I, V>: std::cmp::Eq,
{
    type Output = OrdMap<I, V>;
    fn dirty(&mut self, _edge: &<E::AnchorHandle as AnchorHandle>::Token) {
        // self.vals = None;
        self.dirty = true
    }

    fn poll_updated<G: UpdateContext<Engine = E>>(&mut self, ctx: &mut G) -> Poll {
        if self.dirty {
            let pending_exists = self
                .anchors
                .iter()
                .any(|(_i, anchor)| ctx.request(anchor, true) == Poll::Pending);
            if pending_exists {
                return Poll::Pending;
            }
            let new_vals: Option<OrdMap<I, V>> = Some(
                self.anchors
                    .iter()
                    .map(|(i, anchor)| (i.clone(), ctx.get(anchor).clone()))
                    .collect(),
            );

            if self.vals != new_vals {
                self.vals = new_vals;
                return Poll::Updated;
            }
        }
        self.dirty = false;
        Poll::Unchanged
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
        Some(("VectorCollect", self.location))
    }
}

#[cfg(test)]
mod test {

    use im_rc::OrdMap;

    use crate::{collections::ord_map_methods::Dict, dict, singlethread::*};

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

        let bw = bcut.map(|v| {
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
