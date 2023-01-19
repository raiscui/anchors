/*
 * @Author: Rais
 * @Date: 2022-07-11 22:12:01
 * @LastEditTime: 2023-01-19 16:19:40
 * @LastEditors: Rais
 * @Description:
 */

use im_rc::OrdSet;
use tracing::trace;

use crate::expert::{
    Anchor, AnchorHandle, AnchorInner, Engine, OutputContext, Poll, UpdateContext,
};
use std::panic::Location;

impl<I: 'static + Clone + std::cmp::Ord, E: Engine> std::iter::FromIterator<Anchor<I, E>>
    for Anchor<OrdSet<I>, E>
where
    <E as Engine>::AnchorHandle: PartialOrd + Ord,
{
    fn from_iter<T>(iter: T) -> Self
    where
        T: IntoIterator<Item = Anchor<I, E>>,
    {
        OrdSetCollect::new(iter.into_iter().collect())
    }
}

impl<'a, I: 'static + Clone + std::cmp::Ord, E: Engine> std::iter::FromIterator<&'a Anchor<I, E>>
    for Anchor<OrdSet<I>, E>
where
    <E as Engine>::AnchorHandle: PartialOrd + Ord,
{
    fn from_iter<T>(iter: T) -> Self
    where
        T: IntoIterator<Item = &'a Anchor<I, E>>,
    {
        OrdSetCollect::new(iter.into_iter().cloned().collect())
    }
}

struct OrdSetCollect<T, E: Engine> {
    anchors: OrdSet<Anchor<T, E>>,
    vals: Option<OrdSet<T>>,
    dirty: bool,
    location: &'static Location<'static>,
}

impl<T: 'static + Clone + std::cmp::Ord, E: Engine> OrdSetCollect<T, E>
where
    <E as Engine>::AnchorHandle: PartialOrd + Ord,
{
    #[track_caller]
    pub fn new(anchors: OrdSet<Anchor<T, E>>) -> Anchor<OrdSet<T>, E> {
        E::mount(Self {
            anchors,
            vals: None,
            dirty: true,
            location: Location::caller(),
        })
    }
}

impl<T: 'static + Clone + std::cmp::Ord, E: Engine> AnchorInner<E> for OrdSetCollect<T, E>
where
    <E as Engine>::AnchorHandle: PartialOrd + Ord,
{
    type Output = OrdSet<T>;
    fn dirty(&mut self, _edge: &<E::AnchorHandle as AnchorHandle>::Token) {
        // self.vals = None;
        self.dirty = true
    }

    fn poll_updated<G: UpdateContext<Engine = E>>(&mut self, ctx: &mut G) -> Poll {
        if self.dirty {
            let pending_exists = self
                .anchors
                .iter()
                .any(|anchor| ctx.request(anchor, true) == Poll::Pending);
            if pending_exists {
                return Poll::Pending;
            }
            let new_vals = Some(
                self.anchors
                    .iter()
                    .map(|anchor| ctx.get(anchor).clone())
                    .collect(),
            );
            if self.vals != new_vals {
                self.vals = new_vals;
                trace!("Anchor<OrdSet> Updated");
                return Poll::Updated;
            }
        }
        self.dirty = false;
        trace!("Anchor<OrdSet> Unchanged");
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

    use im_rc::{ordset, OrdSet};

    use crate::singlethread::*;

    #[test]
    fn find_change() {
        let mut engine = Engine::new();

        let a = Var::new(1);
        let aw = a.watch();
        println!("{aw:#?}");
        a.set(2);
        println!("{aw:#?}");
    }

    #[test]
    fn collect() {
        let mut engine = Engine::new();
        let a = Var::new(1);
        let b = Var::new(2);
        let c = Var::new(5);

        let nums: Anchor<OrdSet<_>> = ordset![a.watch(), b.watch(), c.watch()]
            .into_iter()
            .collect();
        let sum: Anchor<usize> = nums.map(|nums| nums.iter().sum());
        let ns: Anchor<usize> = nums.map(|nums: &OrdSet<_>| nums.len());

        assert_eq!(engine.get(&sum), 8);
        assert_eq!(engine.get(&sum), 8);
        println!("----get 2 times");
        a.set(2);
        assert_eq!(engine.get(&sum), 7); //TODO because sets [2,2] comb to [2]  ,     [2,5]
        println!("----will set c");

        c.set(1);
        println!("----set 2 times get 1 times");

        assert_eq!(engine.get(&sum), 3); // [2,1]
        println!("----get 2 times, ns {}", engine.get(&ns));
        b.set(9);
        println!("----after b set: {}", engine.get(&sum)); // [2,1,9]
        println!("----after b set: {}", engine.get(&sum)); // [2,1,9]
        println!("----will set b");
        b.set(9);
        println!("----set b over");

        println!("----after b set2: {}", engine.get(&sum));
        println!("----after b set2: {}", engine.get(&sum));
    }
}
