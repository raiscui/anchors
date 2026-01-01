/*
 * @Author: Rais
 * @Date: 2023-04-03 14:30:01
 * @LastEditTime: 2025-12-09 01:17:15
 * @LastEditors: Rais
 * @Description:
 */

use crate::im_rc::ordmap;

use crate::{
    // collections::ord_map_collect::OrdMapCollect,
    expert::{
        Anchor, AnchorHandle, AnchorInner, Engine, OutputContext, Poll, UpdateContext, ValOrAnchor,
        constant::Constant,
    },
    im_rc::OrdMap,
};
// ─────────────────────────────────────────────────────────────────────────────

// ╔══════════════════════════════════════════════════════════════════════════════╗
// ║ 支持从静态 VOA 字典/迭代器直接构建 Anchor<OrdMap<..>>，统一走流式收集，规避重入 ║
// ╚══════════════════════════════════════════════════════════════════════════════╝
impl<I, V, E> From<OrdMap<I, ValOrAnchor<V, E>>> for Anchor<OrdMap<I, V>, E>
where
    <E as Engine>::AnchorHandle: PartialOrd + Ord,
    V: std::clone::Clone + 'static + std::cmp::PartialEq,
    I: 'static + Clone + std::cmp::Ord,
    E: Engine,
{
    #[track_caller]
    fn from(value: OrdMap<I, ValOrAnchor<V, E>>) -> Self {
        OrdMapVOACollectStream::from_value(value)
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
    #[track_caller]
    fn from(value: Anchor<OrdMap<I, ValOrAnchor<V, E>>, E>) -> Self {
        OrdMapVOACollectStream::new_to_anchor(value)
    }
}

impl<I, V, E> std::iter::FromIterator<(I, ValOrAnchor<V, E>)> for Anchor<OrdMap<I, V>, E>
where
    <E as Engine>::AnchorHandle: PartialOrd + Ord,
    V: std::clone::Clone + 'static + std::cmp::PartialEq,
    I: 'static + Clone + std::cmp::Ord,
    E: Engine,
{
    #[track_caller]
    fn from_iter<T>(iter: T) -> Self
    where
        T: IntoIterator<Item = (I, ValOrAnchor<V, E>)>,
    {
        let dict: OrdMap<I, ValOrAnchor<V, E>> = iter.into_iter().collect();
        OrdMapVOACollectStream::from_value(dict)
    }
}

impl<'a, I, V, E> std::iter::FromIterator<&'a (I, ValOrAnchor<V, E>)> for Anchor<OrdMap<I, V>, E>
where
    <E as Engine>::AnchorHandle: PartialOrd + Ord,
    V: std::clone::Clone + 'static + std::cmp::PartialEq,
    I: 'static + Clone + std::cmp::Ord,
    E: Engine,
{
    #[track_caller]
    fn from_iter<T>(iter: T) -> Self
    where
        T: IntoIterator<Item = &'a (I, ValOrAnchor<V, E>)>,
    {
        let dict: OrdMap<I, ValOrAnchor<V, E>> = iter.into_iter().cloned().collect();
        OrdMapVOACollectStream::from_value(dict)
    }
}

impl<'a, I, V, E> std::iter::FromIterator<(&'a I, &'a ValOrAnchor<V, E>)>
    for Anchor<OrdMap<I, V>, E>
where
    <E as Engine>::AnchorHandle: PartialOrd + Ord,
    V: std::clone::Clone + 'static + std::cmp::PartialEq,
    I: 'static + Clone + std::cmp::Ord,
    E: Engine,
{
    #[track_caller]
    fn from_iter<T>(iter: T) -> Self
    where
        T: IntoIterator<Item = (&'a I, &'a ValOrAnchor<V, E>)>,
    {
        let dict: OrdMap<I, ValOrAnchor<V, E>> = iter
            .into_iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        OrdMapVOACollectStream::from_value(dict)
    }
}

// 过渡器：将 `ValOrAnchor` 预先“固化”为稳定的 Anchor，再复用已有的 `OrdMapCollect`。
// 好处：依赖图在构造时就成型，避免在 `poll_updated` 过程中临时变更依赖链引发重入。
// pub struct OrdMapVOACollect;

// impl OrdMapVOACollect {
//     /// 将 `OrdMap<I, ValOrAnchor<V>>` 映射为 `OrdMap<I, Anchor<V>>`，然后直接交给 `OrdMapCollect`
//     /// 处理，保证依赖在构造期就稳定下来。
//     #[track_caller]
//     pub fn new_to_anchor<I, V, E>(anchors: OrdMap<I, ValOrAnchor<V, E>>) -> Anchor<OrdMap<I, V>, E>
//     where
//         <E as Engine>::AnchorHandle: PartialOrd + Ord,
//         V: Clone + 'static + std::cmp::PartialEq,
//         I: 'static + Clone + std::cmp::Ord,
//         E: Engine,
//     {
//         let anchor_map = Self::voa_dict_to_anchor_dict(anchors);
//         OrdMapCollect::new_to_anchor(anchor_map)
//     }

//     /// 把 VOA 字典转换为“稳定 Anchor”字典：`Val` 立即包成 Constant，`Anchor` 原样传递。
//     fn voa_dict_to_anchor_dict<I, V, E>(
//         anchors: OrdMap<I, ValOrAnchor<V, E>>,
//     ) -> OrdMap<I, Anchor<V, E>>
//     where
//         V: Clone + 'static + std::cmp::PartialEq,
//         I: Clone + std::cmp::Ord,
//         E: Engine,
//     {
//         let pool = ordmap::OrdMapPool::new(anchors.len());
//         let mut dict = OrdMap::with_pool(&pool);

//         anchors
//             .into_iter()
//             .map(|(i, voa)| {
//                 let anchor = match voa {
//                     ValOrAnchor::Val(v) => Constant::new_internal::<E>(v),
//                     ValOrAnchor::Anchor(an) => an,
//                 };
//                 (i, anchor)
//             })
//             .collect_into(&mut dict);
//         dict
//     }
// }

/// 流式收集：Anchor 条目直接 request 读取，Val 条目直接取值，不在持锁期生成新的 Anchor，从根上回避重入。
pub struct OrdMapVOACollectStream<I, V, E: Engine> {
    input: Anchor<OrdMap<I, ValOrAnchor<V, E>>, E>,
    vals: Option<OrdMap<I, V>>,
    dirty: bool,
    location: &'static std::panic::Location<'static>,
}

impl<I: 'static + Clone + std::cmp::Ord, V, E: Engine> OrdMapVOACollectStream<I, V, E>
where
    <E as Engine>::AnchorHandle: PartialOrd + Ord,
    V: std::clone::Clone + 'static + std::cmp::PartialEq,
{
    /// 将静态 VOA 字典包成 Constant，再统一走流式收集器，避免两套实现分叉。
    #[track_caller]
    pub fn from_value(dict: OrdMap<I, ValOrAnchor<V, E>>) -> Anchor<OrdMap<I, V>, E> {
        // 先固化输入为 Anchor，依赖图在构造期成型，后续仅需 request/get。
        let input = Constant::new_internal::<E>(dict);
        Self::new_to_anchor(input)
    }

    #[track_caller]
    pub fn new_to_anchor(
        input: Anchor<OrdMap<I, ValOrAnchor<V, E>>, E>,
    ) -> Anchor<OrdMap<I, V>, E> {
        E::mount(Self {
            input,
            vals: None,
            dirty: true,
            location: std::panic::Location::caller(),
        })
    }
}

impl<I, V, E> AnchorInner<E> for OrdMapVOACollectStream<I, V, E>
where
    <E as Engine>::AnchorHandle: PartialOrd + Ord,
    V: std::clone::Clone + 'static + std::cmp::PartialEq,
    I: 'static + Clone + std::cmp::Ord,
    E: Engine,
{
    type Output = OrdMap<I, V>;

    fn dirty(&mut self, _edge: &<E::AnchorHandle as AnchorHandle>::Token) {
        self.dirty = true;
    }

    fn poll_updated<G: UpdateContext<Engine = E>>(&mut self, ctx: &mut G) -> Poll {
        let mut changed = false;

        if self.dirty {
            match ctx.request(&self.input, true) {
                Poll::Pending | Poll::PendingDefer | Poll::PendingInvalidToken => {
                    return Poll::Pending;
                }
                Poll::Updated | Poll::Unchanged => {
                    // Updated 直接继续重建。
                }
            };

            let dict_in = ctx.get(&self.input);
            let entries: Vec<(I, ValOrAnchor<V, E>)> = dict_in
                .iter()
                .map(|(k, voa)| (k.clone(), voa.clone()))
                .collect();
            // 先遍历并请求所有子 Anchor，避免遇到 Pending 直接早退导致需要多次调度。
            let mut pending_child = false;
            let mut ready_entries: Vec<(I, V)> = Vec::with_capacity(entries.len());

            for (k, voa) in entries {
                match voa {
                    ValOrAnchor::Val(v) => {
                        ready_entries.push((k, v));
                    }
                    ValOrAnchor::Anchor(an) => {
                        match ctx.request(&an, true) {
                            Poll::Pending | Poll::PendingDefer | Poll::PendingInvalidToken => {
                                pending_child = true;
                                continue;
                            }
                            _ => {}
                        }
                        ready_entries.push((k, ctx.get(&an).clone()));
                    }
                }
            }

            if pending_child {
                // 有未就绪子节点，保持 dirty 状态，等待下一轮统一重算。
                return Poll::Pending;
            }

            let pool = ordmap::OrdMapPool::new(ready_entries.len());
            let mut dict = OrdMap::with_pool(&pool);
            ready_entries.into_iter().collect_into(&mut dict);

            self.vals = Some(dict);
            self.dirty = false;
            changed = true;
        } else {
            match ctx.request(&self.input, true) {
                Poll::Pending | Poll::PendingDefer | Poll::PendingInvalidToken => {
                    return Poll::Pending;
                }
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

    fn debug_location(&self) -> Option<(&'static str, &'static std::panic::Location<'static>)> {
        Some(("DictCollectVOAStream", self.location))
    }
}

#[cfg(test)]
mod test {

    use crate::{collections::ord_map_methods::Dict, im_rc::OrdMap};

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

        // let nums: Anchor<OrdMap<usize, usize>> = f.watch().into();

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
