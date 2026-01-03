use crate::expert::{
    Anchor, AnchorHandle, AnchorInner, Engine, OutputContext, Poll, UpdateContext,
};
use std::panic::Location;

impl<I: 'static + Clone, E: Engine> std::iter::FromIterator<Anchor<I, E>> for Anchor<Vec<I>, E> {
    fn from_iter<T>(iter: T) -> Self
    where
        T: IntoIterator<Item = Anchor<I, E>>,
    {
        VecCollect::new_to_anchor(iter.into_iter().collect())
    }
}

impl<'a, I: 'static + Clone, E: Engine> std::iter::FromIterator<&'a Anchor<I, E>>
    for Anchor<Vec<I>, E>
{
    fn from_iter<T>(iter: T) -> Self
    where
        T: IntoIterator<Item = &'a Anchor<I, E>>,
    {
        VecCollect::new_to_anchor(iter.into_iter().cloned().collect())
    }
}

struct VecCollect<T, E: Engine> {
    anchors: Vec<Anchor<T, E>>,
    vals: Option<Vec<T>>,
    location: &'static Location<'static>,
    dirty: bool,
}

impl<T: 'static + Clone, E: Engine> VecCollect<T, E> {
    #[track_caller]
    pub fn new_to_anchor(anchors: Vec<Anchor<T, E>>) -> Anchor<Vec<T>, E> {
        E::mount(Self {
            anchors,
            vals: None,
            location: Location::caller(),
            dirty: true,
        })
    }
}

impl<T: 'static + Clone, E: Engine> AnchorInner<E> for VecCollect<T, E> {
    type Output = Vec<T>;
    fn dirty(&mut self, _edge: &<E::AnchorHandle as AnchorHandle>::Token) {
        // self.vals = None;
        self.dirty = true;
    }

    fn poll_updated<G: UpdateContext<Engine = E>>(&mut self, ctx: &mut G) -> Poll {
        let mut changed = false;
        if self.dirty {
            ////////////////////////////////////////////////////////////////////////////////
            // 与 `VectorVOACollect` 同理：`PendingInvalidToken` 不是可恢复的 Pending。
            // 若继续把它当 Pending，会导致 VecCollect 每轮 stabilize 都重复 request 死 token。
            //
            // 策略：把失效 token 对应的 Anchor 视为“已移除”，从 anchors 列表里剔除，
            // 并基于剩余 anchors 立即重建输出，让图能收敛。
            ////////////////////////////////////////////////////////////////////////////////
            let mut invalid_found = false;
            let mut polls: Vec<(Poll, &Anchor<T, E>)> = Vec::with_capacity(self.anchors.len());
            let mut next_anchors: Vec<Anchor<T, E>> = Vec::with_capacity(self.anchors.len());

            for anchor in self.anchors.iter() {
                let s = ctx.request(anchor, true);
                match s {
                    Poll::Pending | Poll::PendingDefer => return Poll::Pending,
                    Poll::PendingInvalidToken => {
                        invalid_found = true;
                    }
                    _ => {
                        polls.push((s, anchor));
                        next_anchors.push(anchor.clone());
                    }
                }
            }
            self.dirty = false;

            if invalid_found {
                self.anchors = next_anchors;
                self.vals = Some(
                    self.anchors
                        .iter()
                        .map(|anchor| ctx.get(anchor).clone())
                        .collect(),
                );
                return Poll::Updated;
            }

            // ─────────────────────────────────────────────────────────────────────────────
            // self.anchors.iter().for_each(|a| {
            //     let s = ctx.request(a, true);
            //     println!("{s:?}")
            // });
            // ─────────────────────────────────────────────────────

            if let Some(ref mut old_vals) = self.vals {
                for (old_val, (poll, anchor)) in old_vals.iter_mut().zip(polls.iter()) {
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
        Some(("VecCollect", self.location))
    }
}

#[cfg(test)]
mod test {
    use crate::singlethread::*;
    #[test]
    fn collect() {
        let mut engine = Engine::new();
        let a = Var::new(1);
        let b = Var::new(2);
        let c = Var::new(5);
        let nums: Anchor<Vec<_>> = vec![a.watch(), b.watch(), c.watch()].into_iter().collect();
        let sum: Anchor<usize> = nums.map(|nums| nums.iter().sum());
        let ns: Anchor<usize> = nums.map(|nums: &Vec<_>| nums.len());

        assert_eq!(engine.get(&sum), 8);

        a.set(2);
        assert_eq!(engine.get(&sum), 9);

        c.set(1);
        assert_eq!(engine.get(&sum), 5);
        println!("ns {}", engine.get(&ns));
    }
}
