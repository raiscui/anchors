use crate::im_rc::Vector;
use crate::im_rc::vector;

use crate::expert::{
    Anchor, AnchorHandle, AnchorInner, Engine, OutputContext, Poll, UpdateContext,
};
use std::panic::Location;

use super::ord_map_methods::Dict;

impl<I, V, E> From<Anchor<Dict<I, Anchor<V, E>>, E>> for Anchor<Vector<V>, E>
where
    <E as Engine>::AnchorHandle: PartialOrd + Ord,
    V: std::clone::Clone + 'static,
    I: 'static + Clone + std::cmp::Ord,
    E: Engine,
    // OrdMap<I, V>: std::cmp::Eq,
{
    #[track_caller]
    fn from(value: Anchor<Dict<I, Anchor<V, E>>, E>) -> Self {
        value.then(|v| VectorCollect::new_to_anchor(v.values().cloned().collect()))
    }
}

impl<V, E> From<&Anchor<Vector<Anchor<V, E>>, E>> for Anchor<Vector<V>, E>
where
    <E as Engine>::AnchorHandle: PartialOrd + Ord,
    V: std::clone::Clone + 'static,
    E: Engine,
    // OrdMap<I, V>: std::cmp::Eq,
{
    #[track_caller]
    fn from(value: &Anchor<Vector<Anchor<V, E>>, E>) -> Self {
        value.then(|v| VectorCollect::new_to_anchor(v.clone()))
    }
}
impl<V, E> From<Anchor<Vector<Anchor<V, E>>, E>> for Anchor<Vector<V>, E>
where
    <E as Engine>::AnchorHandle: PartialOrd + Ord,
    V: std::clone::Clone + 'static,
    E: Engine,
    // OrdMap<I, V>: std::cmp::Eq,
{
    #[track_caller]
    fn from(value: Anchor<Vector<Anchor<V, E>>, E>) -> Self {
        value.then(|v| VectorCollect::new_to_anchor(v.clone()))
    }
}
impl<V, E> From<Vector<Anchor<V, E>>> for Anchor<Vector<V>, E>
where
    <E as Engine>::AnchorHandle: PartialOrd + Ord,
    V: std::clone::Clone + 'static,
    E: Engine,
    // OrdMap<I, V>: std::cmp::Eq,
{
    #[track_caller]
    fn from(value: Vector<Anchor<V, E>>) -> Self {
        VectorCollect::new_to_anchor(value)
    }
}

impl<I: 'static + Clone, E: Engine> std::iter::FromIterator<Anchor<I, E>> for Anchor<Vector<I>, E> {
    #[track_caller]
    fn from_iter<T>(iter: T) -> Self
    where
        T: IntoIterator<Item = Anchor<I, E>>,
    {
        VectorCollect::new_to_anchor(iter.into_iter().collect())
    }
}

impl<'a, I: 'static + Clone, E: Engine> std::iter::FromIterator<&'a Anchor<I, E>>
    for Anchor<Vector<I>, E>
{
    #[track_caller]
    fn from_iter<T>(iter: T) -> Self
    where
        T: IntoIterator<Item = &'a Anchor<I, E>>,
    {
        VectorCollect::new_to_anchor(iter.into_iter().cloned().collect())
    }
}

pub struct VectorCollect<T, E: Engine> {
    anchors: Vector<Anchor<T, E>>,
    vals: Option<Vector<T>>,
    location: &'static Location<'static>,
    dirty: bool,
    /// 复用的临时缓冲区：用于记录每个 anchor 的 Poll（与 `self.anchors` 对齐）。
    ///
    /// 性能动机：
    /// - `VectorCollect` 常出现在高频链路（例如 event_matching / scene 构建）；
    /// - 若每次 dirty 都 `Vec::with_capacity(len)`，会让 allocator 变成 Top 热点；
    /// - 这里用“结构体字段 + clear()”复用容量，尽量避免重复分配。
    polls_buf: Vec<Poll>,
}

impl<T: 'static + Clone, E: Engine> VectorCollect<T, E> {
    #[track_caller]
    pub fn new_to_anchor(anchors: Vector<Anchor<T, E>>) -> Anchor<Vector<T>, E> {
        E::mount(Self {
            anchors,
            vals: None,
            location: Location::caller(),
            dirty: true,
            polls_buf: Vec::new(),
        })
    }
}

impl<T: 'static + Clone, E: Engine> AnchorInner<E> for VectorCollect<T, E> {
    type Output = Vector<T>;
    fn dirty(&mut self, _child: &<E::AnchorHandle as AnchorHandle>::Token) {
        // self.vals = None;
        self.dirty = true;
    }

    fn poll_updated<G: UpdateContext<Engine = E>>(&mut self, ctx: &mut G) -> Poll {
        let mut changed = false;
        if self.dirty {
            ////////////////////////////////////////////////////////////////////////////////
            // `PendingInvalidToken` 不会变成 Ready。
            // 若仍当作 Pending，会导致 VectorCollect 持续 request 失效 token，图无法收敛。
            //
            // 策略：将失效 token 对应的 Anchor 从 anchors 列表中剔除，再重建输出。
            ////////////////////////////////////////////////////////////////////////////////
            let mut invalid_found = false;
            self.polls_buf.clear();
            {
                // 复用容量：只 reserve，不 shrink，避免反复分配。
                let need = self.anchors.len().saturating_sub(self.polls_buf.capacity());
                if need > 0 {
                    self.polls_buf.reserve(need);
                }
            }

            for anchor in self.anchors.iter() {
                let poll = ctx.request(anchor, true);
                if poll.is_waiting() {
                    // NOTE:
                    // - 仍保持 dirty=true；下次 poll 会继续尝试收敛。
                    return Poll::Pending;
                }
                if poll == Poll::PendingInvalidToken {
                    invalid_found = true;
                }
                self.polls_buf.push(poll);
            }
            self.dirty = false;

            if invalid_found {
                let valid_len = self
                    .polls_buf
                    .iter()
                    .filter(|p| *p != &Poll::PendingInvalidToken)
                    .count();

                let pool = vector::RRBPool::<Anchor<T, E>>::new(valid_len);
                let mut next_anchors = Vector::with_pool(&pool);
                for (anchor, poll) in self.anchors.iter().zip(self.polls_buf.iter()) {
                    if poll == &Poll::PendingInvalidToken {
                        continue;
                    }
                    next_anchors.push_back(anchor.clone());
                }

                let pool = vector::RRBPool::<T>::new(next_anchors.len());
                let mut vals = Vector::with_pool(&pool);
                next_anchors
                    .iter()
                    .map(|anchor| ctx.get(anchor).clone())
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
                for ((old_val, poll), anchor) in old_vals
                    .iter_mut()
                    .zip(self.polls_buf.iter())
                    .zip(self.anchors.iter())
                {
                    if poll == &Poll::Updated {
                        *old_val = ctx.get(anchor).clone();
                        changed = true;
                    }
                }
            } else {
                let pool = vector::RRBPool::<T>::new(self.anchors.len());
                let mut vals = Vector::with_pool(&pool);
                self.anchors
                    .iter()
                    .map(|anchor| ctx.get(anchor).clone())
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
        Some(("VectorCollect", self.location))
    }
}

#[cfg(test)]
mod test {

    use crate::im_rc::{Vector, vector};

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
        let nums: Anchor<Vector<_>> = vector![a.watch(), bw, c.watch()].into_iter().collect();
        let sum: Anchor<usize> = nums.map(|nums| nums.iter().sum());
        let ns: Anchor<usize> = nums.map(|nums: &Vector<_>| nums.len());

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
