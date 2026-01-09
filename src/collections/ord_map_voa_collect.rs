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
use std::collections::BTreeSet;
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
    /// 记录曾经遇到过的失效 key，避免在输入未变化时反复 request 死 token 造成刷屏。
    invalid_keys: BTreeSet<I>,
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
            invalid_keys: BTreeSet::new(),
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
        // ─────────────────────────────────────────────────────────────
        // anchors 契约说明（非常重要）：
        // - `poll_updated` 只有在“本轮 request 了某个未就绪子 Anchor”时才允许返回 Pending。
        // - 过去这里存在一个分支：`input Updated => dirty=true; return Pending`。
        //   该分支没有 request 未就绪子 Anchor，会触发引擎的 unexpected_pending/panic，
        //   也可能导致“冻结旧输出”从而造成界面不更新。
        // - 正确做法：当 input Updated 时，必须在同一轮直接进入重建逻辑。
        // ─────────────────────────────────────────────────────────────
        // 1) 先 request 输入字典，确保依赖关系/必要性正确，同时判定是否需要重建。
        let input_poll = ctx.request(&self.input, true);
        if input_poll.is_waiting() {
            return Poll::Pending;
        }
        match input_poll {
            Poll::PendingInvalidToken => {
                // 输入 token 失效属于“无法恢复的依赖丢失”，交给上层/引擎做冻结或 panic（无历史输出）。
                return Poll::PendingInvalidToken;
            }
            Poll::Updated => {
                // 输入字典已更新：即使本节点未收到 dirty 通知，也必须重建。
                self.dirty = true;
                // 输入已变化：之前记录的 invalid key 可能被替换为新的有效 Anchor，需要清空后重新判定。
                self.invalid_keys.clear();
            }
            Poll::Unchanged | Poll::Pending => {}
            #[cfg(feature = "anchors_pending_queue")]
            Poll::PendingDefer => {}
        };

        // 2) 若无需重建，则保持 Unchanged。
        if !self.dirty {
            return Poll::Unchanged;
        }

        // 3) 重建输出：Val 直接取值，Anchor 通过 request/get 读取。
        let dict_in = ctx.get(&self.input);
        let entries: Vec<(I, ValOrAnchor<V, E>)> = dict_in
            .iter()
            .map(|(k, voa)| (k.clone(), voa.clone()))
            .collect();
        // 先遍历并 request 所有子 Anchor，避免遇到 Pending 直接早退导致需要多次调度。
        let mut pending_child = false;
        let mut ready_entries: Vec<(I, V)> = Vec::with_capacity(entries.len());

        for (k, voa) in entries {
            // 若 key 已经被判定为 invalid，则跳过，避免重复 request 死 token。
            if self.invalid_keys.contains(&k) {
                continue;
            }
            match voa {
                ValOrAnchor::Val(v) => {
                    ready_entries.push((k, v));
                }
                ValOrAnchor::Anchor(an) => {
                    let poll = ctx.request(&an, true);
                    if poll.is_waiting() {
                        pending_child = true;
                        continue;
                    }
                    if poll.is_invalid_token() {
                        ////////////////////////////////////////////////////////////////////////////////
                        // `PendingInvalidToken` 不会变 Ready：
                        // - 将该 key 视为“元素已被移除”，从输出中剔除；
                        // - 同时缓存 key，避免后续重复 request 造成刷屏。
                        ////////////////////////////////////////////////////////////////////////////////
                        self.invalid_keys.insert(k.clone());
                        ctx.unrequest(&an);
                        continue;
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

    fn debug_location(&self) -> Option<(&'static str, &'static std::panic::Location<'static>)> {
        Some(("DictCollectVOAStream", self.location))
    }
}

#[cfg(test)]
mod test {

    use crate::{collections::ord_map_methods::Dict, im_rc::OrdMap};

    use super::OrdMapVOACollectStream;
    use crate::expert::{
        AnchorHandle, AnchorInner, Engine as AnchorEngine, OutputContext, Poll, UpdateContext,
    };
    use crate::{dict, singlethread::*};
    use std::panic::Location;

    type Token = <<Engine as AnchorEngine>::AnchorHandle as AnchorHandle>::Token;

    /// ////////////////////////////////////////////////////////////////////////////
    /// 回归：防止 “input Updated => dirty=true; return Pending” 这种违规 Pending
    ///
    /// 说明：
    /// - 我们用一个“屏蔽 dirty 转发”的包装器来模拟：输入变了，但收集器内部没收到 dirty 通知。
    /// - 旧实现会在 `dirty=false` 时遇到 `input Updated` 直接返回 Pending，
    ///   引擎会把它当成 unexpected_pending，可能冻结旧输出，导致界面不更新。
    /// - 修复后：必须在同一轮进入重建逻辑，返回 Updated，输出随输入变化而更新。
    /// ////////////////////////////////////////////////////////////////////////////
    struct DirtyDroppedOrdMapVoaCollectStream<I, V> {
        inner: OrdMapVOACollectStream<I, V, Engine>,
        location: &'static Location<'static>,
    }

    impl<I, V> AnchorInner<Engine> for DirtyDroppedOrdMapVoaCollectStream<I, V>
    where
        V: Clone + PartialEq + 'static,
        I: 'static + Clone + Ord,
    {
        type Output = OrdMap<I, V>;

        fn dirty(&mut self, _edge: &Token) {
            // 故意不转发给 inner.dirty，模拟“dirty 通知丢失”。
        }

        fn poll_updated<G: UpdateContext<Engine = Engine>>(&mut self, ctx: &mut G) -> Poll {
            <OrdMapVOACollectStream<I, V, Engine> as AnchorInner<Engine>>::poll_updated(
                &mut self.inner,
                ctx,
            )
        }

        fn output<'slf, 'out, G: OutputContext<'out, Engine = Engine>>(
            &'slf self,
            ctx: &mut G,
        ) -> &'out Self::Output
        where
            'slf: 'out,
        {
            <OrdMapVOACollectStream<I, V, Engine> as AnchorInner<Engine>>::output(&self.inner, ctx)
        }

        fn debug_location(&self) -> Option<(&'static str, &'static Location<'static>)> {
            Some(("DirtyDroppedOrdMapVoaCollectStream", self.location))
        }
    }

    #[test]
    fn voa_collect_stream_should_rebuild_on_input_updated_even_without_dirty_forwarding() {
        let mut engine = Engine::new();

        let a = Var::new(1usize);
        let c = Var::new(10usize);

        let dict_var: Var<Dict<usize, ValOrAnchor<usize>>> =
            Var::new(dict!(1usize=>a.watch().into(), 2usize=>2usize.into()));
        let input = dict_var.watch();

        // 先计算一次，确保有“历史输出”。
        let collected: Anchor<OrdMap<usize, usize>> =
            Engine::mount(DirtyDroppedOrdMapVoaCollectStream {
                inner: OrdMapVOACollectStream {
                    input,
                    vals: None,
                    dirty: true,
                    invalid_keys: std::collections::BTreeSet::new(),
                    location: Location::caller(),
                },
                location: Location::caller(),
            });

        let sum: Anchor<usize> = collected.map(|m| m.values().sum());
        assert_eq!(engine.get(&sum), 3);

        // 更新输入字典（新增 key），但包装器不转发 dirty：
        // - 旧实现会走 `dirty=false + input Updated => Pending`，导致冻结旧输出（仍返回 3）。
        // - 新实现必须在同一轮重建并返回 Updated，得到正确的 13。
        dict_var.set(dict!(
            1usize=>a.watch().into(),
            2usize=>2usize.into(),
            3usize=>c.watch().into()
        ));
        assert_eq!(engine.get(&sum), 13);
    }

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

    #[test]
    fn voa_collect_stream_should_drop_invalid_token_instead_of_pending_forever() {
        ////////////////////////////////////////////////////////////////////////////////
        // 该测试验证一个关键契约：
        // - 当某个子 Anchor request 返回 `PendingInvalidToken` 时，OrdMapVOACollectStream 不能返回 Pending；
        // - 否则会导致每轮 stabilize 都重复 request 死 token，刷屏并且图永远不收敛；
        // - 正确行为是：把该 key 当作“已移除”，从输出中剔除，并缓存 key 以避免重复 request。
        ////////////////////////////////////////////////////////////////////////////////

        use std::any::Any;
        use std::collections::HashMap;

        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        struct DummyToken(u64);

        #[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        struct DummyAnchorHandle {
            token: DummyToken,
        }

        impl AnchorHandle for DummyAnchorHandle {
            type Token = DummyToken;

            fn token(&self) -> Self::Token {
                self.token
            }
        }

        #[derive(Clone)]
        struct DummyDirtyHandle;

        impl crate::expert::DirtyHandle for DummyDirtyHandle {
            fn mark_dirty(&self) {}
        }

        struct DummyEngine;

        impl AnchorEngine for DummyEngine {
            type AnchorHandle = DummyAnchorHandle;
            type DirtyHandle = DummyDirtyHandle;

            fn mount<I: crate::expert::AnchorInner<Self> + 'static>(
                _inner: I,
            ) -> crate::expert::Anchor<I::Output, Self> {
                unreachable!("该测试直接构造 OrdMapVOACollectStream，不需要 mount")
            }
        }

        struct DummyCtx {
            polls: HashMap<DummyToken, Poll>,
            vals: HashMap<DummyToken, Box<dyn Any>>,
            requests: Vec<DummyToken>,
        }

        impl DummyCtx {
            fn new() -> Self {
                Self {
                    polls: HashMap::new(),
                    vals: HashMap::new(),
                    requests: Vec::new(),
                }
            }
        }

        impl UpdateContext for DummyCtx {
            type Engine = DummyEngine;

            fn get<'out, 'slf, O: 'static>(
                &'slf self,
                anchor: &crate::expert::Anchor<O, Self::Engine>,
            ) -> &'out O
            where
                'slf: 'out,
            {
                let token = anchor.token();
                let val = self.vals.get(&token).expect("缺少测试值");
                val.as_ref()
                    .downcast_ref::<O>()
                    .expect("DummyCtx get downcast 失败")
            }

            fn request<O: 'static>(
                &mut self,
                anchor: &crate::expert::Anchor<O, Self::Engine>,
                _necessary: bool,
            ) -> Poll {
                let token = anchor.token();
                self.requests.push(token);
                match self.polls.get(&token) {
                    Some(Poll::Updated) => Poll::Updated,
                    Some(Poll::Unchanged) => Poll::Unchanged,
                    Some(Poll::Pending) => Poll::Pending,
                    #[cfg(feature = "anchors_pending_queue")]
                    Some(Poll::PendingDefer) => Poll::PendingDefer,
                    Some(Poll::PendingInvalidToken) => Poll::PendingInvalidToken,
                    None => Poll::Unchanged,
                }
            }

            fn unrequest<O: 'static>(&mut self, _anchor: &crate::expert::Anchor<O, Self::Engine>) {}

            fn dirty_handle(&mut self) -> <Self::Engine as AnchorEngine>::DirtyHandle {
                DummyDirtyHandle
            }
        }

        let input = crate::expert::Anchor::<
            OrdMap<usize, crate::expert::ValOrAnchor<u32, DummyEngine>>,
            DummyEngine,
        >::new_from_expert(DummyAnchorHandle {
            token: DummyToken(100),
        });
        let a = crate::expert::Anchor::<u32, DummyEngine>::new_from_expert(DummyAnchorHandle {
            token: DummyToken(1),
        });
        let b = crate::expert::Anchor::<u32, DummyEngine>::new_from_expert(DummyAnchorHandle {
            token: DummyToken(2),
        });

        let pool = crate::im_rc::ordmap::OrdMapPool::new(3);
        let mut dict_in: OrdMap<usize, crate::expert::ValOrAnchor<u32, DummyEngine>> =
            OrdMap::with_pool(&pool);
        dict_in.insert(1usize, crate::expert::ValOrAnchor::Val(10u32));
        dict_in.insert(2usize, crate::expert::ValOrAnchor::Anchor(a.clone()));
        dict_in.insert(3usize, crate::expert::ValOrAnchor::Anchor(b.clone()));

        let mut inner = OrdMapVOACollectStream::<usize, u32, DummyEngine> {
            input: input.clone(),
            vals: None,
            dirty: true,
            invalid_keys: std::collections::BTreeSet::new(),
            location: Location::caller(),
        };

        let mut ctx = DummyCtx::new();
        ctx.polls.insert(DummyToken(100), Poll::Unchanged);
        ctx.polls.insert(DummyToken(1), Poll::Unchanged);
        ctx.polls.insert(DummyToken(2), Poll::PendingInvalidToken);

        ctx.vals.insert(DummyToken(100), Box::new(dict_in));
        ctx.vals.insert(DummyToken(1), Box::new(5u32));

        assert_eq!(
            AnchorInner::poll_updated(&mut inner, &mut ctx),
            Poll::Updated
        );
        let out = inner.vals.clone().expect("必须生成输出");

        let pool = crate::im_rc::ordmap::OrdMapPool::new(2);
        let mut expected: OrdMap<usize, u32> = OrdMap::with_pool(&pool);
        expected.insert(1usize, 10u32);
        expected.insert(2usize, 5u32);
        assert_eq!(out, expected);

        // 再次触发重建：不应再次 request 已确认失效的 token(2)。
        ctx.requests.clear();
        inner.dirty = true;
        assert_eq!(
            AnchorInner::poll_updated(&mut inner, &mut ctx),
            Poll::Updated
        );
        assert!(
            !ctx.requests.contains(&DummyToken(2)),
            "不应重复 request 死 token，避免 invalid_token 日志刷屏"
        );
    }
}
