/*
 * @Author: Rais
 * @Date: 2022-09-14 11:08:53
 * @LastEditTime: 2025-11-24 22:52:51
 * @LastEditors: Rais
 * @Description:
 */

use im_rc::ordmap;

use crate::im::OrdMap;

use crate::expert::{
    Anchor, AnchorHandle, AnchorInner, Engine, OutputContext, Poll, UpdateContext,
    constant::Constant,
};
use std::panic::Location;

// ╔══════════════════════════════════════════════════════════════════════════════╗
// ║ 从静态字典或迭代器直接构建 Anchor<OrdMap<..>>，内部统一走流式收集器以规避重入 ║
// ╚══════════════════════════════════════════════════════════════════════════════╝
/// 兼容旧 API：保留类型名，内部委派给流式收集器。
pub struct OrdMapCollect;
impl OrdMapCollect {
    #[track_caller]
    pub fn new_to_anchor<I, V, E>(anchors: OrdMap<I, Anchor<V, E>>) -> Anchor<OrdMap<I, V>, E>
    where
        <E as Engine>::AnchorHandle: PartialOrd + Ord,
        V: Clone + 'static,
        I: 'static + Clone + std::cmp::Ord,
        E: Engine,
    {
        OrdMapCollectStream::from_value(anchors)
    }
}

impl<I, V, E> From<OrdMap<I, Anchor<V, E>>> for Anchor<OrdMap<I, V>, E>
where
    <E as Engine>::AnchorHandle: PartialOrd + Ord,
    V: std::clone::Clone + 'static,
    I: 'static + Clone + std::cmp::Ord,
    E: Engine,
{
    #[track_caller]
    fn from(value: OrdMap<I, Anchor<V, E>>) -> Self {
        // 将静态字典包成 Constant，再交给流式收集器统一处理，保证依赖链稳定
        OrdMapCollectStream::from_value(value)
    }
}

// 保留原 API，但内部使用流式收集器；限制为单线程 Engine 以匹配 Var 的 Engine 类型。
impl<I, V>
    From<Anchor<OrdMap<I, Anchor<V, crate::singlethread::Engine>>, crate::singlethread::Engine>>
    for Anchor<OrdMap<I, V>, crate::singlethread::Engine>
where
    <crate::singlethread::Engine as Engine>::AnchorHandle: PartialOrd + Ord,
    V: std::clone::Clone + 'static,
    I: 'static + Clone + std::cmp::Ord,
{
    #[track_caller]
    fn from(
        value: Anchor<
            OrdMap<I, Anchor<V, crate::singlethread::Engine>>,
            crate::singlethread::Engine,
        >,
    ) -> Self {
        OrdMapCollectStream::new_to_anchor(value)
    }
}

impl<I, V, E> std::iter::FromIterator<(I, Anchor<V, E>)> for Anchor<OrdMap<I, V>, E>
where
    <E as Engine>::AnchorHandle: PartialOrd + Ord,
    V: std::clone::Clone + 'static,
    I: 'static + Clone + std::cmp::Ord,
    E: Engine,
{
    #[track_caller]
    fn from_iter<T>(iter: T) -> Self
    where
        T: IntoIterator<Item = (I, Anchor<V, E>)>,
    {
        let anchors: OrdMap<I, Anchor<V, E>> = iter.into_iter().collect();
        OrdMapCollectStream::from_value(anchors)
    }
}

impl<'a, I, V, E> std::iter::FromIterator<&'a (I, Anchor<V, E>)> for Anchor<OrdMap<I, V>, E>
where
    <E as Engine>::AnchorHandle: PartialOrd + Ord,
    V: std::clone::Clone + 'static,
    I: 'static + Clone + std::cmp::Ord,
    E: Engine,
{
    #[track_caller]
    fn from_iter<T>(iter: T) -> Self
    where
        T: IntoIterator<Item = &'a (I, Anchor<V, E>)>,
    {
        let anchors: OrdMap<I, Anchor<V, E>> = iter.into_iter().cloned().collect();
        OrdMapCollectStream::from_value(anchors)
    }
}

impl<'a, I, V, E> std::iter::FromIterator<(&'a I, &'a Anchor<V, E>)> for Anchor<OrdMap<I, V>, E>
where
    <E as Engine>::AnchorHandle: PartialOrd + Ord,
    V: std::clone::Clone + 'static,
    I: 'static + Clone + std::cmp::Ord,
    E: Engine,
{
    #[track_caller]
    fn from_iter<T>(iter: T) -> Self
    where
        T: IntoIterator<Item = (&'a I, &'a Anchor<V, E>)>,
    {
        let anchors: OrdMap<I, Anchor<V, E>> = iter
            .into_iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        OrdMapCollectStream::from_value(anchors)
    }
}

// pub struct OrdMapCollect<I, V, E: Engine> {
//     anchors: OrdMap<I, Anchor<V, E>>,
//     vals: Option<OrdMap<I, V>>,
//     dirty: bool,
//     location: &'static Location<'static>,
// }

// impl<I: 'static + Clone + std::cmp::Ord, V, E: Engine> OrdMapCollect<I, V, E>
// where
//     <E as Engine>::AnchorHandle: PartialOrd + Ord,
//     V: std::clone::Clone + 'static,
//     //TODO 通过特化 制作PartialEq版本, (im_rc 在 PartialEq 的情况下 对比 ,没有Eq 性能高)
//     //TODO 制作 vector smallvec 的对比版本
//     // OrdMap<I, V>: std::cmp::Eq,
// {
//     #[track_caller]
//     pub fn new_to_anchor(anchors: OrdMap<I, Anchor<V, E>>) -> Anchor<OrdMap<I, V>, E> {
//         E::mount(Self {
//             anchors,
//             vals: None,
//             dirty: true,
//             location: Location::caller(),
//         })
//     }
// }

// impl<I, V, E> AnchorInner<E> for OrdMapCollect<I, V, E>
// where
//     <E as Engine>::AnchorHandle: PartialOrd + Ord,
//     V: std::clone::Clone + 'static,
//     I: 'static + Clone + std::cmp::Ord,
//     E: Engine,
//     // OrdMap<I, V>: std::cmp::Eq,
// {
//     type Output = OrdMap<I, V>;
//     fn dirty(&mut self, _edge: &<E::AnchorHandle as AnchorHandle>::Token) {
//         // self.vals = None;
//         self.dirty = true;
//     }

//     fn poll_updated<G: UpdateContext<Engine = E>>(&mut self, ctx: &mut G) -> Poll {
//         let mut changed = false;

//         if self.dirty {
//             let polls = self
//                 .anchors
//                 .iter()
//                 .try_fold(vec![], |mut acc, (i, anchor)| {
//                     let s = ctx.request(anchor, true);
//                     if s == Poll::Pending {
//                         None
//                     } else {
//                         acc.push((s, (i, anchor)));
//                         Some(acc)
//                     }
//                 });

//             if polls.is_none() {
//                 return Poll::Pending;
//             }

//             self.dirty = false;

//             if let Some(ref mut old_vals) = self.vals {
//                 for (poll, (i, anchor)) in &polls.unwrap() {
//                     if &Poll::Updated == poll {
//                         old_vals.insert((**i).clone(), ctx.get(anchor).clone());
//                         changed = true;
//                     }
//                 }
//             } else {
//                 // self.vals = Some(
//                 //     self.anchors
//                 //         .iter()
//                 //         .map(|(i, anchor)| (i.clone(), ctx.get(anchor).clone()))
//                 //         .collect(),
//                 // );
//                 // changed = true;

//                 let pool = ordmap::OrdMapPool::new(self.anchors.len());
//                 let mut dict = OrdMap::with_pool(&pool);

//                 self.anchors
//                     .iter()
//                     .map(|(i, anchor)| (i.clone(), ctx.get(anchor).clone()))
//                     .collect_into(&mut dict);

//                 self.vals = Some(dict);
//                 changed = true;
//             }
//         }

//         if changed {
//             Poll::Updated
//         } else {
//             Poll::Unchanged
//         }
//     }

//     fn output<'slf, 'out, G: OutputContext<'out, Engine = E>>(
//         &'slf self,
//         _ctx: &mut G,
//     ) -> &'out Self::Output
//     where
//         'slf: 'out,
//     {
//         self.vals.as_ref().unwrap()
//     }

//     fn debug_location(&self) -> Option<(&'static str, &'static Location<'static>)> {
//         Some(("DictCollect", self.location))
//     }
// }

/// 流式版本：只挂一次节点，后续通过输入 Anchor 更新，可增删键且避免 then 的重入风险。
pub struct OrdMapCollectStream<I, V, E: Engine> {
    input: Anchor<OrdMap<I, Anchor<V, E>>, E>,
    vals: Option<OrdMap<I, V>>,
    dirty: bool,
    location: &'static Location<'static>,
}

impl<I: 'static + Clone + std::cmp::Ord, V, E: Engine> OrdMapCollectStream<I, V, E>
where
    <E as Engine>::AnchorHandle: PartialOrd + Ord,
    V: std::clone::Clone + 'static,
{
    #[track_caller]
    pub fn new_to_anchor(input: Anchor<OrdMap<I, Anchor<V, E>>, E>) -> Anchor<OrdMap<I, V>, E> {
        E::mount(Self {
            input,
            vals: None,
            dirty: true,
            location: Location::caller(),
        })
    }

    #[track_caller]
    pub fn from_value(dict: OrdMap<I, Anchor<V, E>>) -> Anchor<OrdMap<I, V>, E> {
        let input = Constant::new_internal::<E>(dict);
        Self::new_to_anchor(input)
    }
}

impl<I, V, E> AnchorInner<E> for OrdMapCollectStream<I, V, E>
where
    <E as Engine>::AnchorHandle: PartialOrd + Ord,
    V: std::clone::Clone + 'static,
    I: 'static + Clone + std::cmp::Ord,
    E: Engine,
{
    type Output = OrdMap<I, V>;
    fn dirty(&mut self, _edge: &<E::AnchorHandle as AnchorHandle>::Token) {
        self.dirty = true;
    }

    fn poll_updated<G: UpdateContext<Engine = E>>(&mut self, ctx: &mut G) -> Poll {
        let mut changed = false;

        if std::env::var("ANCHORS_DEBUG_COLLECT")
            .map(|v| v != "0")
            .unwrap_or(false)
        {
            println!(
                "OrdMapCollectStream poll: dirty={} loc={:?}",
                self.dirty, self.location
            );
        }

        // 只有一个输入：整张字典 Anchor。更新/脏时重建输出。
        if self.dirty {
            match ctx.request(&self.input, true) {
                Poll::Pending | Poll::PendingDefer => {
                    // 输入仍在计算，延后本节点重建，避免在持锁期间重复入队。
                    return Poll::Pending;
                }
                Poll::Updated | Poll::Unchanged => {
                    // Updated 直接继续重建即可（已确保依赖顺序），重入风险低于 Pending。
                }
            };

            let dict_in = ctx.get(&self.input);
            let entries: Vec<(I, Anchor<V, E>)> = dict_in
                .iter()
                .map(|(i, anchor)| (i.clone(), anchor.clone()))
                .collect();
            // 一次性请求全部子 Anchor，避免遇到第一个 Pending 就早退导致多轮调度。
            let mut pending_child = false;
            let mut ready_entries: Vec<(I, V)> = Vec::with_capacity(entries.len());

            for (i, anchor) in entries {
                match ctx.request(&anchor, true) {
                    Poll::Pending | Poll::PendingDefer => {
                        pending_child = true;
                        continue;
                    }
                    _ => {}
                }
                ready_entries.push((i, ctx.get(&anchor).clone()));
            }

            if pending_child {
                // 子节点尚未全部就绪，保持 dirty，等待下一轮统一重建。
                return Poll::Pending;
            }

            let pool = ordmap::OrdMapPool::new(ready_entries.len());
            let mut dict = OrdMap::with_pool(&pool);
            ready_entries.into_iter().collect_into(&mut dict);

            if std::env::var("ANCHORS_DEBUG_COLLECT")
                .map(|v| v != "0")
                .unwrap_or(false)
            {
                println!(
                    "OrdMapCollectStream 重建完成，条目={} loc={:?}",
                    dict.len(),
                    self.location
                );
            }

            self.vals = Some(dict);
            self.dirty = false;
            changed = true;
        } else {
            // 保持依赖以便高度/必要关系正确
            match ctx.request(&self.input, true) {
                Poll::Pending | Poll::PendingDefer => return Poll::Pending,
                Poll::Updated => {
                    self.dirty = true;
                    return Poll::Pending;
                }
                Poll::Unchanged => {}
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
        Some(("DictCollectStream", self.location))
    }
}

#[cfg(test)]
mod test {

    use crate::im::OrdMap;

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

        // let nums: Anchor<OrdMap<_, _>> = f.watch().into();
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

        // let nums2: Anchor<OrdMap<_, _>> = f2.watch().into();
        let nums2 = f2.watch().then(|d| {
            let nums2: Anchor<OrdMap<_, _>> = d.clone().into();
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
