use smallvec::SmallVec;

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
        SmallVecCollect::new_to_anchor(iter.into_iter().collect())
    }
}

impl<'a, I: 'static + Clone, E: Engine, const S: usize> std::iter::FromIterator<&'a Anchor<I, E>>
    for Anchor<SmallVec<[I; S]>, E>
{
    fn from_iter<T>(iter: T) -> Self
    where
        T: IntoIterator<Item = &'a Anchor<I, E>>,
    {
        SmallVecCollect::new_to_anchor(iter.into_iter().cloned().collect())
    }
}

struct SmallVecCollect<T, E: Engine, const S: usize> {
    anchors: SmallVec<[Anchor<T, E>; S]>,
    vals: Option<SmallVec<[T; S]>>,
    location: &'static Location<'static>,
    dirty: bool,
}

impl<T: 'static + Clone, E: Engine, const S: usize> SmallVecCollect<T, E, S> {
    #[track_caller]
    pub fn new_to_anchor(anchors: SmallVec<[Anchor<T, E>; S]>) -> Anchor<SmallVec<[T; S]>, E> {
        E::mount(Self {
            anchors,
            vals: None,
            location: Location::caller(),
            dirty: true,
        })
    }
}

impl<T: 'static + Clone, E: Engine, const S: usize> AnchorInner<E> for SmallVecCollect<T, E, S> {
    type Output = SmallVec<[T; S]>;
    fn dirty(&mut self, _edge: &<E::AnchorHandle as AnchorHandle>::Token) {
        // self.vals = None;
        self.dirty = true;
    }

    fn poll_updated<G: UpdateContext<Engine = E>>(&mut self, ctx: &mut G) -> Poll {
        let mut changed = false;
        if self.dirty {
            let polls = self.anchors.iter().try_fold(vec![], |mut acc, anchor| {
                let s = ctx.request(anchor, true);
                if s != Poll::Pending {
                    acc.push((s, anchor));
                    Some(acc)
                } else {
                    None
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
                for (old_val, (poll, anchor)) in old_vals.iter_mut().zip(polls.unwrap().iter()) {
                    if &Poll::Updated == poll {
                        *old_val = ctx.get(anchor).clone();
                        changed = true;
                    }
                }
            } else {
                self.vals = Some(
                    self.anchors
                        .iter()
                        .map(|anchor| ctx.get(anchor).clone())
                        .collect(),
                );
                changed = true;
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
        Some(("VectorCollect", self.location))
    }
}

#[cfg(test)]
mod test {
    use smallvec::{SmallVec, smallvec};

    use crate::singlethread::*;

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
        let f: SmallVec<[Anchor<usize>; 3]> = smallvec![a.watch(), bw, c.watch()];
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
