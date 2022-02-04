use smallvec::{Array, SmallVec};

use crate::expert::{
    Anchor, AnchorHandle, AnchorInner, Engine, OutputContext, Poll, UpdateContext,
};
use std::panic::Location;

impl<I: 'static + Clone, E: Engine, const S: usize> std::iter::FromIterator<Anchor<I, E>>
    for Anchor<SmallVec<[I; S]>, E>
{
    fn from_iter<T>(iter: T) -> Self
    where
        T: IntoIterator<Item = Anchor<I, E>>,
    {
        SmallVecCollect::new(iter.into_iter().collect())
    }
}

impl<'a, I: 'static + Clone, E: Engine, const S: usize> std::iter::FromIterator<&'a Anchor<I, E>>
    for Anchor<SmallVec<[I; S]>, E>
{
    fn from_iter<T>(iter: T) -> Self
    where
        T: IntoIterator<Item = &'a Anchor<I, E>>,
    {
        SmallVecCollect::new(iter.into_iter().cloned().collect())
    }
}

struct SmallVecCollect<T, E: Engine, const S: usize> {
    anchors: SmallVec<[Anchor<T, E>; S]>,
    vals: Option<SmallVec<[T; S]>>,
    location: &'static Location<'static>,
}

impl<T: 'static + Clone, E: Engine, const S: usize> SmallVecCollect<T, E, S> {
    #[track_caller]
    pub fn new(anchors: SmallVec<[Anchor<T, E>; S]>) -> Anchor<SmallVec<[T; S]>, E> {
        E::mount(Self {
            anchors,
            vals: None,
            location: Location::caller(),
        })
    }
}

impl<T: 'static + Clone, E: Engine, const S: usize> AnchorInner<E> for SmallVecCollect<T, E, S> {
    type Output = SmallVec<[T; S]>;
    fn dirty(&mut self, _edge: &<E::AnchorHandle as AnchorHandle>::Token) {
        self.vals = None;
    }

    fn poll_updated<G: UpdateContext<Engine = E>>(&mut self, ctx: &mut G) -> Poll {
        if self.vals.is_none() {
            let pending_exists = self
                .anchors
                .iter()
                .any(|anchor| ctx.request(anchor, true) == Poll::Pending);
            if pending_exists {
                return Poll::Pending;
            }
            self.vals = Some(
                self.anchors
                    .iter()
                    .map(|anchor| ctx.get(anchor)
                    .clone())
                    .collect(),
            )
        }
        Poll::Updated
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
    use crate::singlethread::*;
    use smallvec::SmallVec;

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
        let f: SmallVec<[Anchor<usize>; 3]> = smallvec::smallvec![a.watch(), bw, c.watch()];
        let nums: Anchor<SmallVec<[usize; 3]>> = f.into_iter().collect();
        let sum: Anchor<usize> = nums.map(|nums| nums.iter().sum());
        let ns: Anchor<usize> = nums.map(|nums: &SmallVec<_>| nums.len());

        assert_eq!(engine.get(&sum), 8);

        a.set(2);
        assert_eq!(engine.get(&sum), 9);

        c.set(1);
        assert_eq!(engine.get(&sum), 5);
        println!("ns {}", engine.get(&ns));
        b.set(9);
        println!("after b set: {}", engine.get(&sum));
        b.set(9);
        println!("after b set2: {}", engine.get(&sum));
    }
}
