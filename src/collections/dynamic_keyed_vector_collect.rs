//! DynamicKeyedVectorCollect：按 “key + order” 收集子 Anchor 输出到 `Vector<T>`。
//!
//! 设计目标（OpenSpec add-dynamic-collect）：
//! - **order 变化不拆树**：输入 order 更新时，不应切换输出 Anchor（避免子图被 unrequest/重建）。
//! - **keyed cache**：同一个 key 对应的子 Anchor 只创建一次，后续复用。
//! - **O(changes)**：尽量把代价收敛到增量变化，而不是每次全量重建。
//! - **invalid token 收敛**：
//!   - 子 Anchor request 返回 `PendingInvalidToken` 时，视为该元素已被移除；
//!   - 必须 `unrequest` 并从输出中剔除；
//!   - 同时记录 invalid key，避免后续重复 request 死 token 导致 stabilize 自旋。
//!
//! 典型用法：
//! - Graph 的 outgoing(Render) 变化时，按 EdgeIndex 保序收集 child scenes；
//! - event_matching 递归按 Render Tree 保序收集 child_descs / child callbacks。

use crate::im_rc::Vector;
use crate::im_rc::vector;

use crate::expert::{
    Anchor, AnchorHandle, AnchorInner, Engine, OutputContext, Poll, UpdateContext,
};
use emg_hasher::std::{HashMap, HashSet};
use std::hash::Hash;
use std::panic::Location;
use std::rc::Rc;

/// 按 “key + order” 做 keyed 复用的 Vector 收集器。
///
/// - 输入：`order`（一组 key，顺序即输出顺序）
/// - 产出：`Vector<V>`（与 order 对齐，且会过滤 invalid key）
pub struct DynamicKeyedVectorCollect<K, V, E: Engine> {
    /// key 的顺序来源（决定输出顺序）
    order: Anchor<Vector<K>, E>,
    /// key -> 子 Anchor 的构造器（只在首次见到 key 时调用）
    make_anchor: Rc<dyn Fn(&K) -> Anchor<V, E>>,

    location: &'static Location<'static>,
    dirty: bool,

    /// 已确认 “token 无效” 的 key：后续即使出现在 order 中也会被跳过。
    ///
    /// 说明：
    /// - `PendingInvalidToken` 不会变 Ready；
    /// - 若不做该集合，会导致重复 request 死 token，稳定性与性能都会崩。
    invalid_keys: HashSet<K>,

    /// keyed cache：key -> 子 Anchor
    cache: HashMap<K, Anchor<V, E>>,

    /// 当前生效的顺序（已过滤 invalid_keys）
    order_keys: Vec<K>,
    /// 与 `order_keys` 对齐的子 Anchor 列表
    order_anchors: Vec<Anchor<V, E>>,

    /// 当前输出
    vals: Option<Vector<V>>,

    /// 复用的临时缓冲区：每个子 Anchor 的 Poll（与 `order_anchors` 对齐）
    polls_buf: Vec<Poll>,
    /// 复用的 Vector pool：用于构造 `vals`
    vals_pool: vector::RRBPool<V>,
}

impl<K, V, E> DynamicKeyedVectorCollect<K, V, E>
where
    K: Clone + Eq + Hash + 'static,
    V: Clone + 'static,
    E: Engine,
{
    /// 使用默认 pool 创建收集器。
    ///
    /// 注意：
    /// - 若你处在高频重建链路（例如 `then(|outs| ...)`），建议改用
    ///   [`Self::new_to_anchor_with_vals_pool`] 注入外部共享的 `RRBPool`，减少短命分配。
    #[track_caller]
    pub fn new_to_anchor(
        order: Anchor<Vector<K>, E>,
        make_anchor: impl Fn(&K) -> Anchor<V, E> + 'static,
    ) -> Anchor<Vector<V>, E> {
        // 默认值：给一个中等的 pool，避免每次重建都从 0 开始分配 chunk。
        let vals_pool = vector::RRBPool::<V>::new(256);
        Self::new_to_anchor_with_vals_pool(order, vals_pool, make_anchor)
    }

    /// 创建收集器，并显式指定用于 `vals` 的 `RRBPool`。
    #[track_caller]
    pub fn new_to_anchor_with_vals_pool(
        order: Anchor<Vector<K>, E>,
        vals_pool: vector::RRBPool<V>,
        make_anchor: impl Fn(&K) -> Anchor<V, E> + 'static,
    ) -> Anchor<Vector<V>, E> {
        E::mount(Self {
            order,
            make_anchor: Rc::new(make_anchor),
            location: Location::caller(),
            dirty: true,
            invalid_keys: HashSet::default(),
            cache: HashMap::default(),
            order_keys: Vec::new(),
            order_anchors: Vec::new(),
            vals: None,
            polls_buf: Vec::new(),
            vals_pool,
        })
    }
}

impl<K, V, E> AnchorInner<E> for DynamicKeyedVectorCollect<K, V, E>
where
    K: Clone + Eq + Hash + 'static,
    V: Clone + 'static,
    E: Engine,
{
    type Output = Vector<V>;

    fn dirty(&mut self, _child: &<E::AnchorHandle as AnchorHandle>::Token) {
        self.dirty = true;
    }

    fn poll_updated<G: UpdateContext<Engine = E>>(&mut self, ctx: &mut G) -> Poll {
        ////////////////////////////////////////////////////////////////////////////////
        // 1) 先 request order，确保依赖关系/必要性正确，并判定是否需要重建顺序。
        ////////////////////////////////////////////////////////////////////////////////
        let order_poll = ctx.request(&self.order, true);
        if order_poll.is_waiting() {
            // order 仍未 ready：只能 Pending（此时 output 不可用）
            return Poll::Pending;
        }

        match order_poll {
            Poll::PendingInvalidToken => {
                ////////////////////////////////////////////////////////////////////////////////
                // order token 失效：按“输入被移除”处理（输出清空 + unrequest 全部子 Anchor），
                // 避免持续 request 死 token 导致 stabilize 自旋。
                ////////////////////////////////////////////////////////////////////////////////
                ctx.unrequest(&self.order);

                for (_, anchor) in self.cache.drain() {
                    ctx.unrequest(&anchor);
                }
                self.order_keys.clear();
                self.order_anchors.clear();

                let pool = self.vals_pool.clone();
                self.vals = Some(Vector::with_pool(&pool));
                self.dirty = false;
                return Poll::Updated;
            }
            Poll::Updated => {
                // 即使没收到 dirty 通知，也必须重算（与 OrdMapCollectStream 契约一致）
                self.dirty = true;
            }
            Poll::Unchanged | Poll::Pending => {}
            #[cfg(feature = "anchors_pending_queue")]
            Poll::PendingDefer => {}
        }

        if !self.dirty {
            return Poll::Unchanged;
        }

        ////////////////////////////////////////////////////////////////////////////////
        // 2) reconcile order：keyed cache 复用，不因 reorder 拆树。
        ////////////////////////////////////////////////////////////////////////////////
        let desired = ctx.get(&self.order).clone();

        let mut next_set: HashSet<K> = HashSet::default();
        let mut next_keys: Vec<K> = Vec::with_capacity(desired.len());
        let mut next_anchors: Vec<Anchor<V, E>> = Vec::with_capacity(desired.len());

        for key in desired.iter() {
            if self.invalid_keys.contains(key) {
                continue;
            }

            next_set.insert(key.clone());
            next_keys.push(key.clone());

            let anchor = if let Some(existing) = self.cache.get(key) {
                existing.clone()
            } else {
                let created = (self.make_anchor)(key);
                self.cache.insert(key.clone(), created.clone());
                created
            };
            next_anchors.push(anchor);
        }

        // 移除 cache 中不再出现的 key（并 unrequest）
        if !self.cache.is_empty() {
            let removed: Vec<K> = self
                .cache
                .keys()
                .filter(|k| !next_set.contains(*k))
                .cloned()
                .collect();
            for key in removed {
                if let Some(anchor) = self.cache.remove(&key) {
                    ctx.unrequest(&anchor);
                }
            }
        }

        let mut order_changed = next_keys != self.order_keys;
        if order_changed {
            self.order_keys = next_keys;
            self.order_anchors = next_anchors;
        }

        ////////////////////////////////////////////////////////////////////////////////
        // 3) request 子 Anchor 并处理 invalid token
        ////////////////////////////////////////////////////////////////////////////////
        self.polls_buf.clear();
        {
            // 复用容量：只 reserve，不 shrink，避免反复分配
            let need = self
                .order_anchors
                .len()
                .saturating_sub(self.polls_buf.capacity());
            if need > 0 {
                self.polls_buf.reserve(need);
            }
        }

        let mut pending_child = false;
        let mut invalid_indices: Vec<usize> = Vec::new();

        for (idx, anchor) in self.order_anchors.iter().enumerate() {
            let poll = ctx.request(anchor, true);
            if poll.is_waiting() {
                pending_child = true;
            }
            if poll.is_invalid_token() {
                invalid_indices.push(idx);
            }
            self.polls_buf.push(poll);
        }

        if !invalid_indices.is_empty() {
            ////////////////////////////////////////////////////////////////////////////////
            // 失效 token：按“元素被移除”处理（unrequest + 缓存 invalid key + 从顺序中剔除）
            ////////////////////////////////////////////////////////////////////////////////
            for idx in invalid_indices.into_iter().rev() {
                let key = self.order_keys.remove(idx);
                let anchor = self.order_anchors.remove(idx);

                self.invalid_keys.insert(key.clone());
                self.cache.remove(&key);
                ctx.unrequest(&anchor);
            }

            // 顺序变化 => output 必须重建
            order_changed = true;
        }

        if pending_child {
            // 子节点未全部 ready：保持 dirty，等待下一轮收敛。
            self.dirty = true;
            return Poll::Pending;
        }

        ////////////////////////////////////////////////////////////////////////////////
        // 4) 生成/更新输出
        ////////////////////////////////////////////////////////////////////////////////
        let mut changed = false;
        if order_changed || self.vals.is_none() {
            // 需要重建 output（顺序变化 / 首次输出）
            let pool = self.vals_pool.clone();
            let mut vals = Vector::with_pool(&pool);
            for anchor in self.order_anchors.iter() {
                vals.push_back(ctx.get(anchor).clone());
            }
            self.vals = Some(vals);
            changed = true;
        } else if let Some(ref mut old_vals) = self.vals {
            // 顺序未变：只更新 Updated 的元素
            if old_vals.len() != self.order_anchors.len() {
                // 防御：长度不一致时直接重建（避免越界/错位）
                let pool = self.vals_pool.clone();
                let mut vals = Vector::with_pool(&pool);
                for anchor in self.order_anchors.iter() {
                    vals.push_back(ctx.get(anchor).clone());
                }
                *old_vals = vals;
                changed = true;
            } else {
                for ((old, poll), anchor) in old_vals
                    .iter_mut()
                    .zip(self.polls_buf.iter())
                    .zip(self.order_anchors.iter())
                {
                    if poll == &Poll::Updated {
                        *old = ctx.get(anchor).clone();
                        changed = true;
                    }
                }
            }
        }

        self.dirty = false;
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
        self.vals
            .as_ref()
            .expect("DynamicKeyedVectorCollect: output 在未 ready 时被调用")
    }

    fn debug_location(&self) -> Option<(&'static str, &'static Location<'static>)> {
        Some(("DynamicKeyedVectorCollect", self.location))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests（Dummy Engine + DummyCtx，避免依赖 singlethread 引擎细节）
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::DynamicKeyedVectorCollect;
    use crate::expert::{
        Anchor as ExpertAnchor, AnchorHandle, AnchorInner, Engine as EngineTrait, Poll,
        UpdateContext,
    };
    use crate::im_rc::Vector;
    use crate::im_rc::vector;
    use emg_hasher::std::HashMap;
    use std::any::Any;
    use std::cell::Cell;
    use std::panic::Location;

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    struct DummyToken(u64);

    #[derive(Clone)]
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
    impl EngineTrait for DummyEngine {
        type AnchorHandle = DummyAnchorHandle;
        type DirtyHandle = DummyDirtyHandle;

        fn mount<I: crate::expert::AnchorInner<Self> + 'static>(
            _inner: I,
        ) -> crate::expert::Anchor<I::Output, Self> {
            unreachable!("测试直接构造 DynamicKeyedVectorCollect，不需要 mount")
        }
    }

    struct DummyCtx {
        polls: HashMap<DummyToken, Poll>,
        // 仅支持 O=Vector<u32> 或 O=u32
        vals: HashMap<DummyToken, Box<dyn Any>>,
        requested: Vec<DummyToken>,
        unrequested: Vec<DummyToken>,
    }

    impl DummyCtx {
        fn new() -> Self {
            Self {
                polls: HashMap::default(),
                vals: HashMap::default(),
                requested: Vec::new(),
                unrequested: Vec::new(),
            }
        }
    }

    impl UpdateContext for DummyCtx {
        type Engine = DummyEngine;

        fn get<'out, 'slf, O: 'static>(
            &'slf self,
            anchor: &ExpertAnchor<O, Self::Engine>,
        ) -> &'out O
        where
            'slf: 'out,
        {
            let token = anchor.token();
            let any = self.vals.get(&token).expect("缺少测试值");
            any.downcast_ref::<O>().expect("DummyCtx get 类型不匹配")
        }

        fn request<O: 'static>(
            &mut self,
            anchor: &ExpertAnchor<O, Self::Engine>,
            _necessary: bool,
        ) -> Poll {
            let token = anchor.token();
            self.requested.push(token);
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

        fn unrequest<O: 'static>(&mut self, anchor: &ExpertAnchor<O, Self::Engine>) {
            self.unrequested.push(anchor.token());
        }

        fn dirty_handle(&mut self) -> <Self::Engine as EngineTrait>::DirtyHandle {
            DummyDirtyHandle
        }
    }

    fn order_anchor(token: DummyToken) -> ExpertAnchor<Vector<u32>, DummyEngine> {
        ExpertAnchor::new_from_expert(DummyAnchorHandle { token })
    }

    fn value_anchor(token: DummyToken) -> ExpertAnchor<u32, DummyEngine> {
        ExpertAnchor::new_from_expert(DummyAnchorHandle { token })
    }

    #[test]
    fn reorder_reuses_child_anchors_and_only_reorders_output() {
        let order = order_anchor(DummyToken(1));

        let created_count = std::rc::Rc::new(Cell::new(0u32));
        let created_count_for_closure = std::rc::Rc::clone(&created_count);

        let make_anchor = move |key: &u32| {
            created_count_for_closure.set(created_count_for_closure.get() + 1);
            // token 与 key 绑定，便于在 DummyCtx 中预先塞入值
            value_anchor(DummyToken(100 + u64::from(*key)))
        };

        let vals_pool = vector::RRBPool::<u32>::new(16);
        let mut inner = DynamicKeyedVectorCollect::<u32, u32, DummyEngine> {
            order: order.clone(),
            make_anchor: std::rc::Rc::new(make_anchor),
            location: Location::caller(),
            dirty: true,
            invalid_keys: Default::default(),
            cache: Default::default(),
            order_keys: Vec::new(),
            order_anchors: Vec::new(),
            vals: None,
            polls_buf: Vec::new(),
            vals_pool,
        };

        let mut ctx = DummyCtx::new();
        // 初始 order = [1,2,3]
        ctx.polls.insert(DummyToken(1), Poll::Updated);
        ctx.vals
            .insert(DummyToken(1), Box::new(vector![1u32, 2, 3]));

        // child anchors 预置为 Ready + 给值（与 token 绑定）
        ctx.polls.insert(DummyToken(101), Poll::Unchanged);
        ctx.vals.insert(DummyToken(101), Box::new(10u32));
        ctx.polls.insert(DummyToken(102), Poll::Unchanged);
        ctx.vals.insert(DummyToken(102), Box::new(20u32));
        ctx.polls.insert(DummyToken(103), Poll::Unchanged);
        ctx.vals.insert(DummyToken(103), Box::new(30u32));

        // poll：应创建 3 个 child，并生成 output
        assert_eq!(
            AnchorInner::poll_updated(&mut inner, &mut ctx),
            Poll::Updated
        );
        assert_eq!(created_count.get(), 3, "首次应只创建 3 个 child anchors");
        assert_eq!(inner.vals.clone().unwrap(), vector![10u32, 20, 30]);

        // reorder：order = [3,2,1]
        ctx.polls.insert(DummyToken(1), Poll::Updated);
        ctx.vals
            .insert(DummyToken(1), Box::new(vector![3u32, 2, 1]));
        inner.dirty = true;

        // 注意：reorder 不应再次调用 make_anchor（即不应创建新 child）
        assert_eq!(
            AnchorInner::poll_updated(&mut inner, &mut ctx),
            Poll::Updated
        );
        assert_eq!(
            created_count.get(),
            3,
            "reorder 不应创建新 child anchors（必须复用 keyed cache）"
        );

        // 输出应仅重排
        assert_eq!(inner.vals.clone().unwrap(), vector![30u32, 20, 10]);
    }

    #[test]
    fn removed_key_unrequests_child_anchor() {
        let order = order_anchor(DummyToken(1));
        let make_anchor = move |key: &u32| value_anchor(DummyToken(200 + u64::from(*key)));

        let vals_pool = vector::RRBPool::<u32>::new(16);
        let mut inner = DynamicKeyedVectorCollect::<u32, u32, DummyEngine> {
            order: order.clone(),
            make_anchor: std::rc::Rc::new(make_anchor),
            location: Location::caller(),
            dirty: true,
            invalid_keys: Default::default(),
            cache: Default::default(),
            order_keys: Vec::new(),
            order_anchors: Vec::new(),
            vals: None,
            polls_buf: Vec::new(),
            vals_pool,
        };

        let mut ctx = DummyCtx::new();
        // order = [1,2,3]
        ctx.polls.insert(DummyToken(1), Poll::Updated);
        ctx.vals
            .insert(DummyToken(1), Box::new(vector![1u32, 2, 3]));
        // child anchors 预置为 Ready + 给值
        ctx.polls.insert(DummyToken(201), Poll::Unchanged);
        ctx.vals.insert(DummyToken(201), Box::new(10u32));
        ctx.polls.insert(DummyToken(202), Poll::Unchanged);
        ctx.vals.insert(DummyToken(202), Box::new(20u32));
        ctx.polls.insert(DummyToken(203), Poll::Unchanged);
        ctx.vals.insert(DummyToken(203), Box::new(30u32));

        assert_eq!(
            AnchorInner::poll_updated(&mut inner, &mut ctx),
            Poll::Updated
        );
        assert!(ctx.unrequested.is_empty(), "未移除 key 前不应 unrequest");

        // pop：order = [1,2]
        let removed_anchor_token = DummyToken(203);
        ctx.polls.insert(DummyToken(1), Poll::Updated);
        ctx.vals.insert(DummyToken(1), Box::new(vector![1u32, 2]));
        inner.dirty = true;
        assert_eq!(
            AnchorInner::poll_updated(&mut inner, &mut ctx),
            Poll::Updated
        );
        assert!(
            ctx.unrequested.contains(&removed_anchor_token),
            "移除 key 必须 unrequest 对应子 Anchor"
        );
    }

    #[test]
    fn append_only_creates_child_anchor_for_new_key() {
        let order = order_anchor(DummyToken(1));

        let created_count = std::rc::Rc::new(Cell::new(0u32));
        let created_count_for_closure = std::rc::Rc::clone(&created_count);

        let make_anchor = move |key: &u32| {
            created_count_for_closure.set(created_count_for_closure.get() + 1);
            // token 与 key 绑定，便于在 DummyCtx 中预先塞入值
            value_anchor(DummyToken(400 + u64::from(*key)))
        };

        let vals_pool = vector::RRBPool::<u32>::new(16);
        let mut inner = DynamicKeyedVectorCollect::<u32, u32, DummyEngine> {
            order: order.clone(),
            make_anchor: std::rc::Rc::new(make_anchor),
            location: Location::caller(),
            dirty: true,
            invalid_keys: Default::default(),
            cache: Default::default(),
            order_keys: Vec::new(),
            order_anchors: Vec::new(),
            vals: None,
            polls_buf: Vec::new(),
            vals_pool,
        };

        let mut ctx = DummyCtx::new();
        // 初始 order = [1,2]
        ctx.polls.insert(DummyToken(1), Poll::Updated);
        ctx.vals.insert(DummyToken(1), Box::new(vector![1u32, 2]));

        // child anchors 预置为 Ready + 给值（与 token 绑定）
        ctx.polls.insert(DummyToken(401), Poll::Unchanged);
        ctx.vals.insert(DummyToken(401), Box::new(10u32));
        ctx.polls.insert(DummyToken(402), Poll::Unchanged);
        ctx.vals.insert(DummyToken(402), Box::new(20u32));

        assert_eq!(
            AnchorInner::poll_updated(&mut inner, &mut ctx),
            Poll::Updated
        );
        assert_eq!(created_count.get(), 2, "首次应只创建 2 个 child anchors");
        assert_eq!(inner.vals.clone().unwrap(), vector![10u32, 20]);
        assert!(ctx.unrequested.is_empty(), "append 之前不应 unrequest");

        // append：order = [1,2,3]
        ctx.polls.insert(DummyToken(1), Poll::Updated);
        ctx.vals
            .insert(DummyToken(1), Box::new(vector![1u32, 2, 3]));
        ctx.polls.insert(DummyToken(403), Poll::Unchanged);
        ctx.vals.insert(DummyToken(403), Box::new(30u32));

        ctx.unrequested.clear();
        inner.dirty = true;
        assert_eq!(
            AnchorInner::poll_updated(&mut inner, &mut ctx),
            Poll::Updated
        );
        assert_eq!(
            created_count.get(),
            3,
            "append 只应为新增 key 创建 1 个 child anchor"
        );
        assert_eq!(inner.vals.clone().unwrap(), vector![10u32, 20, 30]);
        assert!(
            ctx.unrequested.is_empty(),
            "append 不应 unrequest 旧 child anchors"
        );
    }

    #[test]
    fn invalid_child_token_is_removed_and_unrequested_and_not_requested_again() {
        let order = order_anchor(DummyToken(1));
        let make_anchor = move |key: &u32| value_anchor(DummyToken(300 + u64::from(*key)));

        let vals_pool = vector::RRBPool::<u32>::new(16);
        let mut inner = DynamicKeyedVectorCollect::<u32, u32, DummyEngine> {
            order: order.clone(),
            make_anchor: std::rc::Rc::new(make_anchor),
            location: Location::caller(),
            dirty: true,
            invalid_keys: Default::default(),
            cache: Default::default(),
            order_keys: Vec::new(),
            order_anchors: Vec::new(),
            vals: None,
            polls_buf: Vec::new(),
            vals_pool,
        };

        let mut ctx = DummyCtx::new();
        // order = [1,2]
        ctx.polls.insert(DummyToken(1), Poll::Updated);
        ctx.vals.insert(DummyToken(1), Box::new(vector![1u32, 2]));
        // a2 => invalid token（子值在第一次 poll 就会被请求，因此这里提前配置好）
        ctx.polls.insert(DummyToken(301), Poll::Unchanged);
        ctx.vals.insert(DummyToken(301), Box::new(10u32));
        ctx.polls.insert(DummyToken(302), Poll::PendingInvalidToken);

        assert_eq!(
            AnchorInner::poll_updated(&mut inner, &mut ctx),
            Poll::Updated
        );

        // 输出只剩 a1
        assert_eq!(inner.vals.clone().unwrap(), vector![10u32]);
        assert!(
            ctx.unrequested.contains(&DummyToken(302)),
            "invalid token 必须触发 unrequest"
        );

        // 再次 dirty：order 仍包含 key=2，但必须不再 request a2
        ctx.requested.clear();
        inner.dirty = true;
        ctx.polls.insert(DummyToken(1), Poll::Unchanged);
        assert_eq!(
            AnchorInner::poll_updated(&mut inner, &mut ctx),
            Poll::Unchanged
        );
        assert!(
            !ctx.requested.contains(&DummyToken(302)),
            "invalid key 必须被跳过，不应再 request 其子 Anchor"
        );
    }
}
