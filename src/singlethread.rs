//! Singlethread is Anchors' default execution engine. It's a single threaded engine capable of both
//! Adapton-style pull updates and — if `mark_observed` and `mark_unobserved` are used,
//! Incremental-style push updates.
//!
//! As of Semptember 2020, execution overhead per-node sits at around 100ns on this author's Macbook
//! Air, likely somewhat more if single node has a significant number of parents or children. Hopefully
//! this will significantly improve over the coming months.

mod generation;
mod graph2;

#[cfg(test)]
mod test;

use graph2::{Graph2, Graph2Guard, NodeGuard, NodeKey, RecalcState};
#[cfg(debug_assertions)]
use tracing::trace;

pub use graph2::AnchorHandle;
#[cfg(feature = "anchors_slotmap")]
pub use graph2::EpochGuard;
#[cfg(feature = "anchors_slotmap")]
pub use graph2::GcStatsSnapshot;
pub use graph2::NodeKey as AnchorToken;

#[cfg(all(feature = "anchors_slotmap", feature = "anchors_pending_queue"))]
#[derive(Default)]
struct PendingStatsInner {
    total_enqueued_raw: Cell<u64>,
    total_enqueued_unique: Cell<u64>,
    dedup_hits: Cell<u64>,
    total_drained: Cell<u64>,
    max_queue_len: Cell<usize>,
    last_enqueue_len: Cell<usize>,
    last_drain_remaining: Cell<usize>,
    last_drain_drained: Cell<usize>,
}

// ─────────────────────────────────────────────────────────────────────────────
// 失效 token 观测：用于在不崩溃的前提下，避免 warn 日志刷屏，同时保留“首个触发点”。
// - 只在 anchors_slotmap 下启用（token 单调 + free list 复用更常见）。
// - 以 child raw_token 作为 key 做去重与计数；同一个 token 一旦开始刷屏，通常就是同一根因。
// ─────────────────────────────────────────────────────────────────────────────
#[cfg(feature = "anchors_slotmap")]
#[derive(Default)]
struct InvalidTokenLogInner {
    counts: HashMap<u64, u32>,
}

#[cfg(feature = "anchors_slotmap")]
impl InvalidTokenLogInner {
    fn bump(&mut self, raw_token: u64) -> u32 {
        let entry = self.counts.entry(raw_token).or_insert(0);
        *entry = entry.saturating_add(1);
        *entry
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// “意外 Pending”观测：
//
// Anchors 的契约是：
// - 若 `poll_updated` 返回 Pending，则必须是因为它 request 了某个子 Anchor 且该子 Anchor 未 Ready；
// - 否则 Engine 无法知道“它在等什么”，继续 requeue 会导致 stabilize 自旋（CPU 满载）。
//
// 但在真实项目里，我们宁愿“冻结旧输出并记录告警”，也不希望 GUI 直接崩溃。
// 这里用 raw_token 做去重计数，避免同一节点反复刷屏。
// ─────────────────────────────────────────────────────────────────────────────
#[cfg(feature = "anchors_slotmap")]
#[derive(Default)]
struct UnexpectedPendingLogInner {
    counts: HashMap<u64, u32>,
}

#[cfg(feature = "anchors_slotmap")]
impl UnexpectedPendingLogInner {
    fn bump(&mut self, raw_token: u64) -> u32 {
        let entry = self.counts.entry(raw_token).or_insert(0);
        *entry = entry.saturating_add(1);
        *entry
    }
}

/// Pending 队列统计快照，用于在 demo/测试后导出指标。
#[derive(Debug, Clone, Copy, Default)]
pub struct AnchorsPendingStats {
    pub total_enqueued_raw: u64,
    pub total_enqueued_unique: u64,
    pub dedup_hits: u64,
    pub total_drained: u64,
    pub max_queue_len: usize,
    pub last_enqueue_len: usize,
    pub last_drain_remaining: usize,
    pub last_drain_drained: usize,
}

/// GUI/观测用的轻量运行时指标。
#[derive(Debug, Clone, Copy, Default)]
pub struct AnchorsMetrics {
    /// 当前活跃节点数量（仅 slotmap 模式有效）。
    pub active_nodes: usize,
    /// 当前进程 RSS，单位字节。
    pub rss_bytes: u64,
}

/// token 审计信息：用于验证单调递增与删除记录。
#[derive(Debug, Clone, Copy, Default)]
pub struct TokenAudit {
    pub next_token: u64,
    pub last_deleted_token: Option<u64>,
}

/// The main struct of the Anchors library. Represents a single value on the singlethread recomputation graph.
///
/// You should basically never need to create these with `Anchor::new_from_expert`; instead call functions like `Var::new` and `MultiAnchor::map`
/// to create them.
pub type Anchor<T> = crate::expert::Anchor<T, Engine>;

/// An Anchor input that can be mutated by calling a setter function from outside of the Anchors recomputation graph.
pub type VarVOA<T> = crate::expert::VarVOA<T, Engine>;
pub type Var<T> = crate::expert::Var<T, Engine>;
pub type ValOrAnchor<T> = crate::expert::ValOrAnchor<T, Engine>;
pub type ForceOpVOA<T> = crate::expert::ForceOpVOA<T, Engine>;

pub use crate::expert::MultiAnchor;

use crate::expert::{AnchorInner, OutputContext, Poll, UpdateContext};

use emg_hasher::std::HashMap;
use generation::Generation;
use indexmap::{IndexMap, IndexSet};
#[cfg(feature = "anchors_slotmap")]
use libc::RUSAGE_SELF;
#[cfg(feature = "anchors_slotmap")]
use libc::getrusage;
#[cfg(feature = "anchors_slotmap")]
use libc::rusage;
use std::any::Any;
use std::backtrace::Backtrace;
#[cfg(all(feature = "anchors_slotmap", feature = "anchors_pending_queue"))]
use std::cell::Cell;
use std::cell::RefCell;
#[cfg(all(feature = "anchors_slotmap", feature = "anchors_pending_queue"))]
use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::fmt::Write as _;
use std::panic::Location;
use std::rc::Rc;
use std::sync::{Mutex, OnceLock};
// ─────────────────────────────────────────────────────────────────────────────

pub fn into_ea<T>(x: impl Into<ValOrAnchor<T>>) -> ValOrAnchor<T> {
    x.into()
}

thread_local! {
    static DEFAULT_MOUNTER: RefCell<Option<Mounter>> = RefCell::new(None);
}

/// Indicates whether the node is a part of some observed calculation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObservedState {
    /// The node has been marked as observed directly via `mark_observed`.
    Observed,

    /// The node is not marked as observed directly.
    /// However, the node has some descendent that is Observed, and this node has
    /// been recalculated since that descendent become Observed.
    Necessary,

    /// The node is not marked as observed directly.
    /// Additionally, this node either has no Observed descendent, or the chain linking
    /// this node to that Observed descendent has not been recalculated since that
    /// dencendent become observed.
    Unnecessary,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum PendingPriority {
    BackgroundLow = 0,
    StateNormal = 1,
    LayoutHigh = 2,
    RenderCritical = 3,
}

impl PendingPriority {
    fn default_for(request_necessary: bool, parent_is_necessary: bool) -> Self {
        if request_necessary && parent_is_necessary {
            PendingPriority::RenderCritical
        } else if parent_is_necessary {
            PendingPriority::LayoutHigh
        } else if request_necessary {
            PendingPriority::StateNormal
        } else {
            PendingPriority::BackgroundLow
        }
    }

    #[cfg(all(feature = "anchors_slotmap", feature = "anchors_pending_queue"))]
    fn as_u8(self) -> u8 {
        self as u8
    }
}

#[cfg(all(feature = "anchors_slotmap", feature = "anchors_pending_queue"))]
#[derive(Clone, Copy, Debug)]
struct PendingRequest {
    parent: NodeKey,
    child: NodeKey,
    necessary: bool,
    priority: PendingPriority,
}

#[cfg(all(feature = "anchors_slotmap", feature = "anchors_pending_queue"))]
#[derive(Default)]
struct PendingQueue {
    buckets: BTreeMap<PendingPriority, IndexMap<NodeKey, PendingRequest>>,
    index: HashMap<NodeKey, PendingPriority>,
}

#[cfg(all(feature = "anchors_slotmap", feature = "anchors_pending_queue"))]
enum PendingEnqueueOutcome {
    Inserted,
    Replaced,
}

#[cfg(all(feature = "anchors_slotmap", feature = "anchors_pending_queue"))]
impl PendingQueue {
    fn len(&self) -> usize {
        self.index.len()
    }

    fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    fn insert(&mut self, entry: PendingRequest) -> PendingEnqueueOutcome {
        if let Some(old_priority) = self.index.get(&entry.child).copied() {
            let (mut existing, empty_after_remove) = {
                let bucket = self
                    .buckets
                    .get_mut(&old_priority)
                    .expect("priority bucket missing");
                let existing = bucket
                    .shift_remove(&entry.child)
                    .expect("pending entry missing");
                (existing, bucket.is_empty())
            };
            if empty_after_remove {
                self.buckets.remove(&old_priority);
            }
            let target_priority = old_priority.max(entry.priority);
            existing.parent = entry.parent;
            existing.necessary |= entry.necessary;
            existing.priority = target_priority;
            self.buckets
                .entry(target_priority)
                .or_insert_with(IndexMap::new)
                .insert(entry.child, existing);
            self.index.insert(entry.child, target_priority);
            PendingEnqueueOutcome::Replaced
        } else {
            self.index.insert(entry.child, entry.priority);
            self.buckets
                .entry(entry.priority)
                .or_insert_with(IndexMap::new)
                .insert(entry.child, entry);
            PendingEnqueueOutcome::Inserted
        }
    }

    fn pop_front(&mut self) -> Option<PendingRequest> {
        let mut empty_priorities = Vec::new();
        let mut result = None;
        for (&priority, bucket) in self.buckets.iter_mut().rev() {
            if let Some(token) = bucket.keys().next().copied() {
                let entry = bucket.shift_remove(&token).expect("pending entry missing");
                self.index.remove(&token);
                result = Some(entry);
            }
            if bucket.is_empty() {
                empty_priorities.push(priority);
            }
            if result.is_some() {
                break;
            }
        }
        for priority in empty_priorities {
            self.buckets.remove(&priority);
        }
        result
    }

    fn drain_all(&mut self) -> Vec<PendingRequest> {
        let mut drained = Vec::with_capacity(self.len());
        while let Some(entry) = self.pop_front() {
            drained.push(entry);
        }
        drained
    }
}

/// The main execution engine of Singlethread.
pub struct Engine {
    // TODO store Nodes on heap directly?? maybe try for Rc<RefCell<SlotMap>> now
    graph: Rc<Graph2>,
    dirty_marks: Rc<RefCell<Vec<NodeKey>>>,

    // tracks the current stabilization generation; incremented on every stabilize
    generation: Generation,
    #[cfg(all(feature = "anchors_slotmap", feature = "anchors_pending_queue"))]
    pending_requests: Rc<RefCell<PendingQueue>>,
    #[cfg(all(feature = "anchors_slotmap", feature = "anchors_pending_queue"))]
    pending_stats: Rc<PendingStatsInner>,
    #[cfg(feature = "anchors_slotmap")]
    invalid_token_log: Rc<RefCell<InvalidTokenLogInner>>,
    #[cfg(feature = "anchors_slotmap")]
    unexpected_pending_log: Rc<RefCell<UnexpectedPendingLogInner>>,
}

struct Mounter {
    graph: Rc<Graph2>,
    /// 供 `VarVOA::set` 在 dirty_handle 尚未建立时做兜底：直接把 token 推入 dirty_marks。
    ///
    /// 背景：
    /// - 某些 `set` 可能发生在 stabilize 期间、且发生在 Var 第一次被 request/poll 之前；
    /// - 这时 Var 内部还没有 `dirty_handle`，常规 `mark_dirty()` 走不通；
    /// - 但如果该 Var 的值被其它 Anchor 在本轮 stabilize 早些时候读取过，
    ///   就会出现“先算出旧值/0 → 后 set 新值 → 但没有 dirty 触发重算”的中间态窗口。
    ///
    /// 目标：
    /// - 不引入额外的 `Engine::get()`（避免 re-entrant get 改变调度顺序），
    /// - 仍然保证 `set` 能进入 dirty 队列，从而让 stabilize 的“收敛补轮”有机会吃掉它。
    dirty_marks: Rc<RefCell<Vec<NodeKey>>>,
}

impl crate::expert::Engine for Engine {
    type AnchorHandle = AnchorHandle;
    type DirtyHandle = DirtyHandle;

    fn mount<I: AnchorInner<Self> + 'static>(inner: I) -> Anchor<I::Output> {
        DEFAULT_MOUNTER.with(|default_mounter| {
            let mut borrow1 = default_mounter.borrow_mut();
            let this = borrow1
                .as_mut()
                .expect("no engine was initialized. did you call `Engine::new()`?");
            let debug_info = inner.debug_info();
            let handle = this.graph.insert(Box::new(inner), debug_info);
            Anchor::new_from_expert(handle)
        })
    }

    fn fallback_mark_dirty(
        token: <Self::AnchorHandle as crate::expert::AnchorHandle>::Token,
    ) -> bool {
        Self::push_dirty_mark_without_handle(token)
    }

    fn fallback_dirty_handle(
        token: <Self::AnchorHandle as crate::expert::AnchorHandle>::Token,
    ) -> Option<DirtyHandle> {
        DEFAULT_MOUNTER.with(|mounter| {
            let borrowed = mounter.borrow();
            let mounter = borrowed.as_ref()?;
            Some(DirtyHandle {
                num: token,
                dirty_marks: mounter.dirty_marks.clone(),
            })
        })
    }

    ////////////////////////////////////////////////////////////////////////////////
    // 诊断：判断当前线程是否已初始化 Engine（TLS mounter 是否存在）
    ////////////////////////////////////////////////////////////////////////////////
    fn is_engine_initialized() -> bool {
        DEFAULT_MOUNTER.with(|mounter| mounter.borrow().is_some())
    }
}

impl Engine {
    #[cfg(all(feature = "anchors_slotmap", feature = "anchors_pending_queue"))]
    fn debug_pending_enabled() -> bool {
        static ENABLED: OnceLock<bool> = OnceLock::new();
        *ENABLED.get_or_init(|| emg_debug_env::bool_lenient("ANCHORS_DEBUG_PENDING"))
    }

    #[cfg(all(feature = "anchors_slotmap", feature = "anchors_pending_queue"))]
    fn defer_disabled() -> bool {
        static DISABLED: OnceLock<bool> = OnceLock::new();
        *DISABLED.get_or_init(|| emg_debug_env::bool_lenient("ANCHORS_DEFER_DISABLED"))
    }
    #[cfg(feature = "anchors_slotmap")]
    fn current_rss_bytes() -> u64 {
        // getrusage 在 macOS 返回 bytes，Linux 返回 KB；这里统一转换成字节后再比较。
        let mut usage: rusage = unsafe { std::mem::zeroed() };
        let ret = unsafe { getrusage(RUSAGE_SELF, &mut usage) };
        if ret != 0 {
            return 0;
        }
        let raw = usage.ru_maxrss as u64;
        // 简单启发式：若值小于 1e9 则视为 KB 转换。
        if raw < 1_000_000_000 {
            raw.saturating_mul(1024)
        } else {
            raw
        }
    }

    /// Creates a new Engine with maximum height 256.
    pub fn new() -> Self {
        Self::new_with_max_height(256)
    }

    /// Creates a new Engine with a custom maximum height.
    pub fn new_with_max_height(max_height: usize) -> Self {
        let dirty_marks: Rc<RefCell<Vec<NodeKey>>> = Default::default();
        let graph = Rc::new(Graph2::new(max_height));
        let mounter = Mounter {
            graph: graph.clone(),
            dirty_marks: dirty_marks.clone(),
        };
        DEFAULT_MOUNTER.with(|v| *v.borrow_mut() = Some(mounter));
        Self {
            graph,
            dirty_marks,
            generation: Generation::new(),
            #[cfg(all(feature = "anchors_slotmap", feature = "anchors_pending_queue"))]
            pending_requests: Rc::new(RefCell::new(PendingQueue::default())),
            #[cfg(all(feature = "anchors_slotmap", feature = "anchors_pending_queue"))]
            pending_stats: Rc::new(PendingStatsInner::default()),
            #[cfg(feature = "anchors_slotmap")]
            invalid_token_log: Rc::new(RefCell::new(InvalidTokenLogInner::default())),
            #[cfg(feature = "anchors_slotmap")]
            unexpected_pending_log: Rc::new(RefCell::new(UnexpectedPendingLogInner::default())),
        }
    }

    /// 兜底：在没有 dirty_handle 的情况下，直接把 token 推入当前 Engine 的 dirty_marks。
    ///
    /// 说明：
    /// - 该路径主要用于 `VarVOA::set`：当 Var 第一次 poll_updated 尚未发生时，dirty_handle 为 None。
    /// - 若此时 set 发生在 stabilize 期间，且上游已经用旧值计算过，则必须能触发一次重算。
    /// - 这里不做任何 “立刻 stabilize” 的动作，只是把 token 入队，
    ///   让本轮/下一轮 stabilize 统一消费（由 `Engine::stabilize` 控制收敛轮次）。
    pub(crate) fn push_dirty_mark_without_handle(token: NodeKey) -> bool {
        DEFAULT_MOUNTER.with(|mounter| {
            let borrowed = mounter.borrow();
            let Some(mounter) = borrowed.as_ref() else {
                return false;
            };
            mounter.dirty_marks.borrow_mut().push(token);
            true
        })
    }

    /// 输出 pending 队列的聚合指标，便于在 demo/集成测试后记录基线。
    #[must_use]
    pub fn pending_stats_snapshot(&self) -> AnchorsPendingStats {
        #[cfg(all(feature = "anchors_slotmap", feature = "anchors_pending_queue"))]
        {
            let stats = &self.pending_stats;
            return AnchorsPendingStats {
                total_enqueued_raw: stats.total_enqueued_raw.get(),
                total_enqueued_unique: stats.total_enqueued_unique.get(),
                dedup_hits: stats.dedup_hits.get(),
                total_drained: stats.total_drained.get(),
                max_queue_len: stats.max_queue_len.get(),
                last_enqueue_len: stats.last_enqueue_len.get(),
                last_drain_remaining: stats.last_drain_remaining.get(),
                last_drain_drained: stats.last_drain_drained.get(),
            };
        }

        #[cfg(not(all(feature = "anchors_slotmap", feature = "anchors_pending_queue")))]
        {
            AnchorsPendingStats::default()
        }
    }

    /// 输出 active_nodes 与 RSS，便于 GUI 与日志实时显示。
    #[must_use]
    pub fn metrics_snapshot(&self) -> AnchorsMetrics {
        #[cfg(feature = "anchors_slotmap")]
        {
            let active = self.graph.with(|graph| graph.active_nodes());
            let rss = Self::current_rss_bytes();
            return AnchorsMetrics {
                active_nodes: active,
                rss_bytes: rss,
            };
        }

        #[cfg(not(feature = "anchors_slotmap"))]
        {
            AnchorsMetrics::default()
        }
    }

    /// 读取 slotmap GC 计数（gc_skipped / free_skip / pending_free）。
    #[cfg(feature = "anchors_slotmap")]
    #[must_use]
    pub fn slotmap_gc_stats(&self) -> GcStatsSnapshot {
        self.graph.gc_stats_snapshot()
    }

    /// 进入 anchors epoch（读窗口）。
    ///
    /// 在 epoch 期间：
    /// - 禁止 token 的物理回收/复用（nodes.remove -> free list）；
    /// - 只允许把待回收节点 retire 入队，等到 epoch end 再统一 reclaim。
    ///
    /// 典型用法：由事件循环在 “事件派发 + stabilize + render” 外层持有该 guard。
    #[cfg(feature = "anchors_slotmap")]
    #[must_use]
    pub fn enter_epoch(&self) -> EpochGuard {
        EpochGuard::new(self.graph.clone())
    }

    /// 读取 token 计数与最近删除记录，便于验证“单调且不复用”。
    #[must_use]
    pub fn token_audit_snapshot(&self) -> TokenAudit {
        #[cfg(feature = "anchors_slotmap")]
        {
            let next = self.graph.token_counter();
            let last_deleted = self.graph.last_deleted_token();
            return TokenAudit {
                next_token: next,
                last_deleted_token: last_deleted,
            };
        }

        #[cfg(not(feature = "anchors_slotmap"))]
        {
            TokenAudit::default()
        }
    }

    // ─────────────────────────────────────────────────────────────────────────
    // 失效 token 观测与去重

    #[cfg(feature = "anchors_slotmap")]
    fn invalid_token_backtrace_enabled() -> bool {
        static ENABLED: OnceLock<bool> = OnceLock::new();
        *ENABLED.get_or_init(|| emg_debug_env::bool_strict("ANCHORS_INVALID_TOKEN_BACKTRACE"))
    }

    #[cfg(not(feature = "anchors_slotmap"))]
    #[inline]
    fn invalid_token_backtrace_enabled() -> bool {
        false
    }

    #[cfg(feature = "anchors_slotmap")]
    fn bump_invalid_token_count(&self, raw_token: u64) -> u32 {
        let mut slot = self.invalid_token_log.borrow_mut();
        slot.bump(raw_token)
    }

    #[cfg(not(feature = "anchors_slotmap"))]
    #[inline]
    fn bump_invalid_token_count(&self, _raw_token: u64) -> u32 {
        1
    }

    #[inline]
    fn should_log_invalid_token(count: u32) -> bool {
        matches!(count, 1 | 10 | 100) || count % 1000 == 0
    }

    // ─────────────────────────────────────────────────────────────────────────
    // “意外 Pending”观测与去重

    #[cfg(feature = "anchors_slotmap")]
    fn bump_unexpected_pending_count(&self, raw_token: u64) -> u32 {
        let mut slot = self.unexpected_pending_log.borrow_mut();
        slot.bump(raw_token)
    }

    #[cfg(not(feature = "anchors_slotmap"))]
    #[inline]
    fn bump_unexpected_pending_count(&self, _raw_token: u64) -> u32 {
        1
    }

    #[inline]
    fn should_log_unexpected_pending(count: u32) -> bool {
        // 复用同一套“稀疏采样”策略：1/10/100/每 1000 次
        Self::should_log_invalid_token(count)
    }

    #[inline]
    fn expect_node<'gg>(
        &self,
        graph: &Graph2Guard<'gg>,
        key: NodeKey,
        context: &str,
    ) -> graph2::NodeGuard<'gg> {
        graph.get(key).unwrap_or_else(|| {
            panic!(
                "slotmap: 节点已被删除或 token 失效，无法执行操作：{}，token={:?}",
                context, key
            )
        })
    }

    /// Marks an Anchor as observed. All observed nodes will always be brought up-to-date
    /// when *any* Anchor in the graph is retrieved. If you get an output value fairly
    /// often, it's best to mark it as Observed so that Anchors can calculate its
    /// dependencies faster.
    pub fn mark_observed<O: 'static>(&mut self, anchor: &Anchor<O>) {
        self.graph.with(|graph| {
            let node = self.expect_node(&graph, anchor.token(), "mark_observed");
            node.observed.set(true);
            if graph2::recalc_state(node) != RecalcState::Ready {
                graph.queue_recalc(node);
            }
        })
    }

    /// Marks an Anchor as unobserved. If the `anchor` has parents that are necessary
    /// because `anchor` was previously observed, those parents will be unmarked as
    /// necessary.
    pub fn mark_unobserved<O: 'static>(&mut self, anchor: &Anchor<O>) {
        self.graph.with(|graph| {
            let node = self.expect_node(&graph, anchor.token(), "mark_unobserved");
            node.observed.set(false);
            Self::update_necessary_children(node);
        })
    }

    #[cfg(all(feature = "anchors_slotmap", feature = "anchors_pending_queue"))]
    fn enqueue_pending_keys(
        &self,
        parent: NodeKey,
        child: NodeKey,
        necessary: bool,
        priority: PendingPriority,
    ) -> bool {
        if Self::defer_disabled() {
            return false;
        }

        let outcome = {
            let mut queue = self.pending_requests.borrow_mut();
            let outcome = queue.insert(PendingRequest {
                parent,
                child,
                necessary,
                priority,
            });
            let queue_len = queue.len();
            self.pending_stats
                .max_queue_len
                .set(self.pending_stats.max_queue_len.get().max(queue_len));
            self.pending_stats.last_enqueue_len.set(queue_len);
            outcome
        };
        self.pending_stats.total_enqueued_raw.set(
            self.pending_stats
                .total_enqueued_raw
                .get()
                .saturating_add(1),
        );
        match outcome {
            PendingEnqueueOutcome::Inserted => {
                self.pending_stats.total_enqueued_unique.set(
                    self.pending_stats
                        .total_enqueued_unique
                        .get()
                        .saturating_add(1),
                );
            }
            PendingEnqueueOutcome::Replaced => {
                self.pending_stats
                    .dedup_hits
                    .set(self.pending_stats.dedup_hits.get().saturating_add(1));
            }
        }

        if Self::debug_pending_enabled() {
            let queue_len = self.pending_requests.borrow().len();
            tracing::debug!(
                target: "anchors",
                parent_token = parent.raw_token(),
                child_token = child.raw_token(),
                priority = priority.as_u8(),
                queue_len,
                "enqueue pending request"
            );
        }

        true
    }

    #[cfg(not(all(feature = "anchors_slotmap", feature = "anchors_pending_queue")))]
    #[inline]
    fn enqueue_pending_keys(
        &self,
        _parent: NodeKey,
        _child: NodeKey,
        _necessary: bool,
        _priority: PendingPriority,
    ) -> bool {
        false
    }

    fn update_necessary_children(node: NodeGuard<'_>) {
        if Self::check_observed_raw(node) != ObservedState::Unnecessary {
            // we have another parent still observed, so skip this
            return;
        }
        for child in node.drain_necessary_children() {
            // TODO remove from calculation queue if necessary?
            Self::update_necessary_children(child);
        }
    }

    #[cfg(feature = "anchors_slotmap")]
    #[inline]
    fn fast_path_ready_value<O: Clone + 'static>(&self, anchor: &Anchor<O>) -> Option<O> {
        if !self.dirty_marks.borrow().is_empty() {
            return None;
        }
        #[cfg(feature = "anchors_pending_queue")]
        if !self.pending_requests.borrow().is_empty() {
            return None;
        }
        self.graph.with(|graph| {
            if graph.has_recalc_pending() || graph.has_pending_free() {
                return None;
            }
            let node = graph.get(anchor.token())?;
            if graph2::recalc_state(node) != RecalcState::Ready {
                return None;
            }
            if node.anchor_locked.get() {
                return None;
            }
            Some(graph.output_cached(self, node))
        })
    }

    #[inline]
    fn ready_node<'gg>(
        &self,
        graph: &Graph2Guard<'gg>,
        key: NodeKey,
        context: &str,
    ) -> graph2::NodeGuard<'gg> {
        let mut retries: u8 = 0;
        loop {
            let node = self.expect_node(graph, key, context);
            if graph2::recalc_state(node) != RecalcState::Ready {
                graph.queue_recalc(node);
            } else if node.anchor_locked.get() {
                node.anchor_locked.set(false);
                graph.queue_recalc_force(node);
            } else {
                return node;
            }
            self.stabilize0();
            retries = retries.saturating_add(1);
            if retries > 3 {
                panic!(
                    "slotmap: anchor locked or pending after stabilize {:?}",
                    node.debug_info.get()
                );
            }
        }
    }

    #[inline]
    fn read_ready_output<O: Clone + 'static>(
        &self,
        node: graph2::NodeGuard<'_>,
        context: &str,
    ) -> O {
        let target_anchor = unsafe { &*node.anchor.get() };
        let inner = target_anchor.as_ref().unwrap_or_else(|| {
            panic!(
                "slotmap: 节点已被删除或 token 失效，无法执行操作：{}，token={:?}",
                context,
                node.key()
            )
        });
        inner
            .output(&mut EngineContext { engine: self })
            .downcast_ref::<O>()
            .unwrap()
            .clone()
    }

    ////////////////////////////////////////////////////////////////////////////////
    // 汇总当前是否仍有待处理的工作：
    // - dirty_marks: Var/VarVOA::set 注入的脏标记
    // - graph.recalc_pending: 已入队但尚未处理的重算
    // - pending_requests: slotmap+pending_queue 下的延迟请求
    //
    // 作用：
    // - get/get_with 在读取输出后判断是否需要补一轮 stabilize
    ////////////////////////////////////////////////////////////////////////////////
    #[inline]
    fn has_pending_work(&self) -> bool {
        if !self.dirty_marks.borrow().is_empty() {
            return true;
        }
        #[cfg(feature = "anchors_slotmap")]
        {
            let graph_pending = self
                .graph
                .with(|graph| graph.has_recalc_pending() || graph.has_pending_free());
            if graph_pending {
                return true;
            }
        }
        #[cfg(all(feature = "anchors_slotmap", feature = "anchors_pending_queue"))]
        {
            if !self.pending_requests.borrow().is_empty() {
                return true;
            }
        }
        false
    }

    pub fn get<O: Clone + 'static>(&mut self, anchor: &Anchor<O>) -> O {
        #[cfg(feature = "anchors_slotmap")]
        if let Some(val) = self.fast_path_ready_value(anchor) {
            return val;
        }

        ////////////////////////////////////////////////////////////////////////////////
        // 方案A: get 内部稳定化重试
        //
        // 目标:
        // - 当 output 阶段产生新的 dirty_marks 时, 在同一次 get 内再 stabilize 一轮
        // - 避免 "先算 0 -> set 宽高 -> dirty 排队到下一帧" 的中间态被返回
        ////////////////////////////////////////////////////////////////////////////////
        const MAX_STABLE_GET_ROUNDS: usize = 4;
        let mut rounds: usize = 0;
        loop {
            self.stabilize();
            let out = self.graph.with(|graph| {
                let anchor_node = self.ready_node(&graph, anchor.token(), "get::<O> 读取节点");
                self.read_ready_output(anchor_node, "get::<O> 读取节点")
            });

            let pending = self.has_pending_work();
            if !pending || rounds >= MAX_STABLE_GET_ROUNDS {
                if pending && rounds >= MAX_STABLE_GET_ROUNDS {
                    tracing::warn!(
                        target: "anchors",
                        rounds,
                        "get::<O> stabilize 重试达到上限, 仍存在待处理工作"
                    );
                }
                return out;
            }

            rounds = rounds.saturating_add(1);
        }
    }
    pub fn get_with<O: Clone + 'static, F: FnOnce(&O) -> R, R>(
        &mut self,
        anchor: &Anchor<O>,
        func: F,
    ) -> R {
        ////////////////////////////////////////////////////////////////////////////////
        // 方案A: get_with 内部稳定化重试
        //
        // 约束:
        // - FnOnce 只能执行一次,因此必须在稳定后再调用
        // - 若 output 阶段触发 dirty_marks,先重试,最后一次才执行 func
        ////////////////////////////////////////////////////////////////////////////////
        const MAX_STABLE_GET_ROUNDS: usize = 4;
        let mut rounds: usize = 0;
        let mut func = Some(func);
        loop {
            self.stabilize();
            let out = self.graph.with(|graph| {
                let target_node =
                    self.ready_node(&graph, anchor.token(), "get_with 读取 anchor/output");
                let borrowed = unsafe { &*target_node.anchor.get() };
                let o = borrowed
                    .as_ref()
                    .unwrap()
                    .output(&mut EngineContext { engine: self })
                    .downcast_ref::<O>()
                    .unwrap();
                o.clone()
            });

            let pending = self.has_pending_work();
            if !pending || rounds >= MAX_STABLE_GET_ROUNDS {
                if pending && rounds >= MAX_STABLE_GET_ROUNDS {
                    tracing::warn!(
                        target: "anchors",
                        rounds,
                        "get_with stabilize 重试达到上限, 仍存在待处理工作"
                    );
                }
                let f = func.take().expect("get_with: func 已被消费");
                return f(&out);
            }

            rounds = rounds.saturating_add(1);
        }
    }

    /// 读取已 Ready 节点的输出副本，不触发重算，仅在 anchors_slotmap 下可用。
    #[cfg(feature = "anchors_slotmap")]
    pub fn peek_value<O: Clone + 'static>(&self, anchor: &Anchor<O>) -> Option<O> {
        self.graph.with(|graph| {
            let anchor_node = self.expect_node(&graph, anchor.token(), "peek_value 读取节点");
            if anchor_node.anchor_locked.get() {
                return None;
            }
            if graph2::recalc_state(anchor_node) != RecalcState::Ready {
                return None;
            }
            Some(graph.output_cached(self, anchor_node))
        })
    }

    /// 检查 `AnchorToken` 是否仍然指向一个“活节点”。
    ///
    /// 说明：
    /// - 该函数只做 slot_token/anchor 是否存在的快速判断，不触发重算，也不会改写队列。
    /// - 主要用途：上层存在一些 token 的索引/缓存（例如 path -> token 的反查索引、backfill/patch 队列）。
    ///   这些结构会保留“弱引用 token”，它们不保活节点。节点被回收后 token 可能失效或被复用。
    /// - 在把这些弱引用 token 再喂回 `stabilize_subset` 或写回依赖补丁前，先做 liveness 过滤，
    ///   可以显著降低 invalid token request 的扩散概率。
    #[inline]
    pub fn is_token_alive(&self, token: AnchorToken) -> bool {
        self.graph.with(|graph| graph.get(token).is_some())
    }

    pub(crate) fn update_dirty_marks(&mut self) {
        #[cfg(debug_assertions)]
        trace!("update_dirty_marks");
        self.graph.with(|graph| {
            let dirty_marks = std::mem::take(&mut *self.dirty_marks.borrow_mut());
            for dirty in dirty_marks {
                if let Some(node) = graph.get(dirty) {
                    mark_dirty(graph, node, false);
                    // 确保自身也进入重算队列，避免特殊情况下 dirty 未触发队列（例如必要关系缺失）。
                    graph.queue_recalc(node);
                } else {
                    #[cfg(feature = "anchors_slotmap")]
                    tracing::warn!(
                        target: "anchors",
                        "update_dirty_marks: token 已失效或节点已删除，跳过处理 {:?}",
                        dirty
                    );
                }
            }
        })
    }

    /// Ensure any Observed nodes are up-to-date, recalculating dependencies as necessary. You
    /// should rarely need to call this yourself; `Engine::get` calls it automatically.
    pub fn stabilize(&mut self) {
        #[cfg(debug_assertions)]
        trace!("stabilize");
        self.update_dirty_marks();
        self.generation.increment();
        self.stabilize_with_pending_queue();

        ////////////////////////////////////////////////////////////////////////////////
        // 方案A(彻底正确)：让一次 `Engine::get()` 内部的 stabilize 具备“收敛性”
        //
        // 背景：
        // - 在当前架构里，某些节点在 poll_updated 期间会触发 `StateVOA/StateVar::set`，
        //   这些 set 只会把 token 追加到 `dirty_marks`；
        // - 但 `Engine::get()` 只在开头调用一次 stabilize；
        // - 因此同一帧内会出现“先用旧值算出 0 → 再写入宽高 → dirty 需要下一轮 get 才生效”的窗口，
        //   最终导致 present 采样到 size=0 的中间态（表现为新增条目不显示/位置不对）。
        //
        // 解决：
        // - 在本次 stabilize 结束后，若发现 stabilize 期间又产生了新的 dirty_marks，
        //   则继续补一轮 update_dirty_marks + stabilize_with_pending_queue，直到收敛。
        // - 这样 `scene_ctx_sa.get()` 返回的是“本次 get 期间可达的收敛结果”，避免中间态进入 present。
        //
        // 安全阀：
        // - 额外轮次做上限，避免异常情况下 dirty_marks 被持续写入导致事件循环卡死。
        ////////////////////////////////////////////////////////////////////////////////
        const MAX_EXTRA_ROUNDS: usize = 8;
        let mut extra_rounds: usize = 0;
        while !self.dirty_marks.borrow().is_empty() && extra_rounds < MAX_EXTRA_ROUNDS {
            extra_rounds = extra_rounds.saturating_add(1);
            #[cfg(debug_assertions)]
            trace!(
                "stabilize: extra_rounds={} reason=dirty_marks_not_empty",
                extra_rounds
            );
            self.update_dirty_marks();
            self.stabilize_with_pending_queue();
        }

        if !self.dirty_marks.borrow().is_empty() {
            tracing::warn!(
                target: "anchors",
                extra_rounds,
                "stabilize: dirty_marks 仍未收敛（已达额外轮次上限），本轮将返回可能的中间态；建议排查是否存在 stabilize 期间反复 set 的非收敛依赖"
            );
        }
        #[cfg(debug_assertions)]
        trace!("....stabilize");
    }

    /// internal function for stabilization. does not update dirty marks or increment the stabilization number
    fn stabilize0(&self) {
        self.stabilize_with_pending_queue();
    }

    fn stabilize_with_pending_queue(&self) {
        #[cfg(debug_assertions)]
        trace!("stabilize0");
        self.graph.with(|graph| {
            // ─────────────────────────────────────────────────────────────
            // 可选调试：检测 stabilize 是否异常“打转”
            //
            // 背景：
            // - 在某些依赖环/不收敛场景中，recalc 队列会反复 requeue，导致 stabilize 长时间不返回；
            // - GUI 表现：CPU 满载、事件循环无法退出，必须 kill。
            //
            // 用法：
            // - `ANCHORS_DEBUG_SPIN=1` 开启检测（默认关闭，避免影响性能）。
            // - `ANCHORS_DEBUG_SPIN_LIMIT=<N>` 可自定义阈值（默认按 active_nodes * 64 放宽，避免启动阶段误报）。
            //
            // 输出策略：
            // - 不再逐步 println（会产生海量日志并影响时序），只在超过阈值时 panic；
            // - panic 消息包含“最近 N 次出队”的 token + debug_info，便于快速定位闭环。
            // ─────────────────────────────────────────────────────────────
            let spin_debug = emg_debug_env::bool_lenient("ANCHORS_DEBUG_SPIN");
            let mut spin_counter: usize = 0;
            let spin_limit: usize = if spin_debug {
                if let Some(v) = emg_debug_env::str_allow_empty("ANCHORS_DEBUG_SPIN_LIMIT") {
                    v.trim().parse::<usize>().unwrap_or(0)
                } else {
                    #[cfg(feature = "anchors_slotmap")]
                    {
                        // 启动阶段/依赖扩展时，合法的高度调整次数可能远超 active_nodes；
                        // 这里使用更保守的倍数，减少误报。
                        let active_nodes = graph.active_nodes();
                        std::cmp::max(5_000, active_nodes.saturating_mul(64))
                    }
                    #[cfg(not(feature = "anchors_slotmap"))]
                    {
                        5_000
                    }
                }
            } else {
                0
            };

            let mut spin_last: VecDeque<(usize, u64, String)> = VecDeque::with_capacity(32);
            #[cfg(feature = "anchors_slotmap")]
            {
                graph.retry_pending_free();
            }
            while let Some((height, node)) = graph.recalc_pop_next() {
                if spin_debug {
                    spin_counter = spin_counter.saturating_add(1);
                    if spin_last.len() == 32 {
                        spin_last.pop_front();
                    }
                    spin_last.push_back((
                        height,
                        node.key().raw_token(),
                        node.debug_info.get()._to_string(),
                    ));

                    if spin_limit > 0 && spin_counter > spin_limit {
                        let mut msg = String::new();
                        let _ = writeln!(
                            &mut msg,
                            "stabilize0 spin detected: 重算次数超过上限 {spin_limit} (count={spin_counter})"
                        );
                        #[cfg(feature = "anchors_slotmap")]
                        {
                            let active_nodes = graph.active_nodes();
                            let _ = writeln!(&mut msg, "active_nodes={active_nodes}");
                        }
                        let _ = writeln!(&mut msg, "recent_recalc (oldest -> newest):");
                        for (i, (h, tok, info)) in spin_last.iter().enumerate() {
                            let _ = writeln!(
                                &mut msg,
                                "  #{i} height={h} token={tok} node={info}"
                            );
                        }
                        panic!("{msg}");
                    }
                }
                let calculation_complete = if graph2::height(node) == height {
                    #[cfg(debug_assertions)]
                    trace!("recalculate height");

                    // TODO with new graph we can automatically relocate nodes if their height changes
                    // this nodes height is current, so we can recalculate
                    self.recalculate(graph, node)
                } else {
                    // skip calculation, redo at correct height
                    false
                };

                if !calculation_complete {
                    graph.queue_recalc(node);
                }
            }
            #[cfg(feature = "anchors_slotmap")]
            {
                graph.retry_pending_free();
                if emg_debug_env::bool_lenient("ANCHORS_ACTIVE_NODES_LOG") {
                    let rss = Self::current_rss_bytes();
                    let active = graph.active_nodes();
                    let gc_stats = graph.gc_stats();
                    let loss_ppm = gc_stats.loss_ppm();
                    let loss_pct = loss_ppm as f64 / 10.0;
                    let skip_total = gc_stats
                        .gc_skipped
                        .saturating_add(gc_stats.free_skip)
                        .saturating_add(gc_stats.pending_free as u64);
                    tracing::info!(
                        target: "anchors",
                        active_nodes = active,
                        rss_bytes = rss,
                        gc_skipped = gc_stats.gc_skipped,
                        free_skip = gc_stats.free_skip,
                        free_attempts = gc_stats.free_attempts,
                        free_succeeded = gc_stats.free_succeeded,
                        pending_free = gc_stats.pending_free,
                        gc_loss_ppm = loss_ppm,
                        gc_loss_pct = loss_pct,
                        gc_loss_n = skip_total,
                        "anchors.active_nodes snapshot"
                    );
                }
            }
        });
        #[cfg(all(feature = "anchors_slotmap", feature = "anchors_pending_queue"))]
        self.process_pending_requests();
        #[cfg(debug_assertions)]
        trace!("...stabilize0");
    }

    #[cfg(all(feature = "anchors_slotmap", feature = "anchors_pending_queue"))]
    fn process_pending_requests(&self) {
        if Self::defer_disabled() {
            let mut queue = self.pending_requests.borrow_mut();
            queue.buckets.clear();
            queue.index.clear();
            return;
        }

        let outcome = {
            let mut queue = self.pending_requests.borrow_mut();
            if queue.is_empty() {
                return;
            }
            queue.drain_all()
        };
        let drained = outcome;
        let total_to_retry = drained.len();
        if total_to_retry > 0 {
            self.pending_stats.total_drained.set(
                self.pending_stats
                    .total_drained
                    .get()
                    .saturating_add(total_to_retry as u64),
            );
            self.pending_stats.last_drain_drained.set(total_to_retry);
        }

        for req in drained {
            if Self::debug_pending_enabled() {
                tracing::debug!(
                    target: "anchors",
                    parent_token = req.parent.raw_token(),
                    child_token = req.child.raw_token(),
                    priority = req.priority.as_u8(),
                    queue_len = total_to_retry,
                    "drain pending request"
                );
            }

            let mut need_retry = false;
            self.graph.with(|graph| {
                let Some(parent) = graph.get(req.parent) else {
                    return;
                };
                let Some(child) = graph.get(req.child) else {
                    return;
                };

                let mut ctx = EngineContextMut {
                    engine: self,
                    graph,
                    node: parent,
                    pending_on_anchor_get: false,
                    invalid_token_requested: false,
                };
                let poll = ctx.request_node(child, req.necessary);
                if matches!(poll, Poll::PendingDefer) {
                    need_retry = true;
                }
            });

            if need_retry {
                let queue_len = {
                    let mut queue = self.pending_requests.borrow_mut();
                    queue.insert(req);
                    queue.len()
                };
                self.pending_stats
                    .max_queue_len
                    .set(self.pending_stats.max_queue_len.get().max(queue_len));
                self.pending_stats.last_enqueue_len.set(queue_len);
            }
        }

        let remaining = self.pending_requests.borrow().len();
        self.pending_stats.last_drain_remaining.set(remaining);

        // ─────────────────────────────────────────────────────────────
        // 可选调试：打印 pending 队列样本
        //
        // 背景：
        // - 当某些依赖长期处于 PendingDefer（例如依赖环、或依赖链无法收敛）时，
        //   `pending_requests` 会持续 drain -> retry -> reinsert，导致 CPU 满载。
        // - 通过打印 queue 的少量样本（parent/child token + debug_info），能快速定位“打转”的 SCC。
        //
        // 用法：
        // - 设置 `ANCHORS_PENDING_DEBUG_SAMPLE=1` 开启（默认关闭，避免影响正常性能）。
        // ─────────────────────────────────────────────────────────────
        if remaining > 0 && total_to_retry > 0 && Self::debug_pending_queue_sample_enabled() {
            use std::sync::atomic::{AtomicU64, Ordering};

            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let tick = COUNTER.fetch_add(1, Ordering::Relaxed);

            // 轻量限流：每 50 次 drain 打一次样本，避免刷屏。
            // 另外：首次命中也会打印一次，保证“第一次打转”就能拿到线索。
            if tick == 0 || tick % 50 == 0 {
                let sample: Vec<PendingRequest> = {
                    let queue = self.pending_requests.borrow();
                    queue
                        .buckets
                        .values()
                        .flat_map(|bucket| bucket.values())
                        .take(8)
                        .copied()
                        .collect()
                };

                let parents: Vec<NodeKey> = sample.iter().map(|req| req.parent).collect();
                let children: Vec<NodeKey> = sample.iter().map(|req| req.child).collect();
                let parents_debug = self.describe_tokens(&parents);
                let children_debug = self.describe_tokens(&children);

                eprintln!(
                    "[anchors] pending_queue remain={remaining} drained={total_to_retry} sample_n={}",
                    sample.len()
                );
                for (i, req) in sample.iter().enumerate() {
                    let parent_dbg = parents_debug
                        .get(i)
                        .map(String::as_str)
                        .unwrap_or("<unknown>");
                    let child_dbg = children_debug
                        .get(i)
                        .map(String::as_str)
                        .unwrap_or("<unknown>");
                    eprintln!(
                        "[anchors] pending_sample[{i}] priority={} parent_token={} parent={parent_dbg} child_token={} child={child_dbg}",
                        req.priority.as_u8(),
                        req.parent.raw_token(),
                        req.child.raw_token()
                    );
                }
            }
        }
    }

    #[cfg(all(feature = "anchors_slotmap", feature = "anchors_pending_queue"))]
    fn describe_tokens(&self, tokens: &[NodeKey]) -> Vec<String> {
        if tokens.is_empty() {
            return Vec::new();
        }
        self.graph.with(|graph| {
            tokens
                .iter()
                .map(|token| {
                    graph
                        .get(*token)
                        .map(|node| node.debug_info.get()._to_string())
                        .unwrap_or_else(|| format!("#{}", token.raw_token()))
                })
                .collect()
        })
    }

    #[cfg(all(feature = "anchors_slotmap", feature = "anchors_pending_queue"))]
    fn debug_pending_queue_sample_enabled() -> bool {
        use std::sync::OnceLock;

        static ENABLED: OnceLock<bool> = OnceLock::new();
        *ENABLED.get_or_init(|| emg_debug_env::bool_strict("ANCHORS_PENDING_DEBUG_SAMPLE"))
    }

    /// returns false if calculation is still pending
    fn recalculate<'a>(&self, graph: Graph2Guard<'a>, node: NodeGuard<'a>) -> bool {
        let this_anchor = &node.anchor;
        let mut ecx = EngineContextMut {
            engine: self,
            node,
            graph,
            pending_on_anchor_get: false,
            invalid_token_requested: false,
        };
        /// ══════════════════════════════════════════════════════════════════════
        /// 锁严格模式开关：
        /// - 现在由编译特征 `lock_strict` 控制，默认启用；关闭该特征则彻底禁用严格检测。
        /// - 在开启特征时仍可用环境变量调节：`ANCHORS_LOCK_STRICT=0` 临时放宽；缺省或非 0 则保持严格。
        fn lock_strict_enabled() -> bool {
            #[cfg(feature = "lock_strict")]
            {
                static STRICT: OnceLock<bool> = OnceLock::new();
                return *STRICT.get_or_init(|| {
                    // 默认严格（true），仅在显式设置为 0/false/off/no 时关闭。
                    if emg_debug_env::str_allow_empty("ANCHORS_LOCK_STRICT").is_none() {
                        return true;
                    }
                    emg_debug_env::bool_lenient("ANCHORS_LOCK_STRICT")
                });
            }

            #[cfg(not(feature = "lock_strict"))]
            {
                false
            }
        }

        /// ══════════════════════════════════════════════════════════════════════
        /// 可选调试：`ANCHORS_LOCK_TRACE=1` 时记录锁的来源，帮助定位重入根因。
        /// 在 strict 模式下 panic 时，会把上一次持锁时的调用栈附带出来。
        fn lock_trace_enabled() -> bool {
            static TRACE: OnceLock<bool> = OnceLock::new();
            *TRACE.get_or_init(|| emg_debug_env::bool_lenient("ANCHORS_LOCK_TRACE"))
        }

        #[derive(Clone)]
        struct LockTraceRecord {
            /// 仅记录定位信息，避免为每一次锁捕获整段回溯导致 RSS 暴涨。
            info: String,
        }

        fn lock_trace_store(key: u64, info: String) {
            if !lock_trace_enabled() {
                return;
            }
            static TRACE_MAP: OnceLock<Mutex<HashMap<u64, LockTraceRecord>>> = OnceLock::new();
            let record = LockTraceRecord { info: info.clone() };
            TRACE_MAP
                .get_or_init(|| Mutex::new(HashMap::default()))
                .lock()
                .expect("lock trace poisoned")
                .insert(key, record);
            println!("ANCHORS_LOCK_TRACE: lock token={} info={}", key, info);
        }

        fn lock_trace_take(key: u64) -> Option<String> {
            if !lock_trace_enabled() {
                return None;
            }
            static TRACE_MAP: OnceLock<Mutex<HashMap<u64, LockTraceRecord>>> = OnceLock::new();
            let removed = TRACE_MAP
                .get_or_init(|| Mutex::new(HashMap::default()))
                .lock()
                .ok()
                .and_then(|mut map| map.remove(&key));
            if let Some(ref info) = removed {
                println!(
                    "ANCHORS_LOCK_TRACE: unlock token={} info(first line)={}",
                    key,
                    info.info.lines().next().unwrap_or("")
                );
            }
            removed.map(|rec| rec.info)
        }

        fn lock_trace_peek(key: u64) -> Option<String> {
            if !lock_trace_enabled() {
                return None;
            }
            static TRACE_MAP: OnceLock<Mutex<HashMap<u64, LockTraceRecord>>> = OnceLock::new();
            TRACE_MAP
                .get_or_init(|| Mutex::new(HashMap::default()))
                .lock()
                .ok()
                .and_then(|map| {
                    map.get(&key).map(|rec| {
                        // 仅在真正需要报错时才捕获回溯，避免日常路径的高额内存占用。
                        let bt = Backtrace::force_capture();
                        format!("{}\n{:?}", rec.info, bt)
                    })
                })
        }

        struct AnchorLockGuard<'a> {
            trace_key: Option<u64>,
            node_key: NodeKey,
            graph: Graph2Guard<'a>,
        }
        impl<'a> Drop for AnchorLockGuard<'a> {
            fn drop(&mut self) {
                if let Some(node) = self.graph.get(self.node_key) {
                    node.anchor_locked.set(false);
                    if debug_requeue_enabled() {
                        println!(
                            "REQUEUE check node={:?} pending_recalc={}",
                            node.debug_info.get()._to_string(),
                            node.pending_recalc.get()
                        );
                    }
                    // 若锁期间收到“补偿重算”标记，解锁后再入队自身，既避免重入也不丢更新。
                    if node.pending_recalc.get() {
                        if debug_requeue_enabled() {
                            println!(
                                "REQUEUE pending node={:?} state_before={:?} height={}",
                                node.debug_info.get()._to_string(),
                                graph2::recalc_state(node),
                                graph2::height(node)
                            );
                        }
                        node.pending_recalc.set(false);
                        // 强制入队：跳过 locked/Pending 判定。
                        self.graph.queue_recalc_force(node);
                    }
                }
                if let Some(key) = self.trace_key.take() {
                    let _ = lock_trace_take(key);
                }
            }
        }

        if node.anchor_locked.get() {
            // ╔════════════ 警示：遗留锁可能意味着真实重入缺陷 ════════════╗
            // ║ STRICT 模式下直接 panic 揭露问题；默认模式下抢占解锁以避免卡死。 ║
            // ╚═══════════════════════════════════════════════════════════════╝
            if lock_strict_enabled() {
                let trace = lock_trace_peek(node.key().raw_token());
                // 冷路径：把 panic/backtrace 的大块逻辑挪到 out-of-line 函数，减少热路径 i-cache 压力。
                panic_anchor_locked_strict(
                    node.key().raw_token(),
                    node.debug_info.get()._to_string(),
                    trace,
                );
            }
            let retry = node.recalc_retry.get().saturating_add(1);
            node.recalc_retry.set(retry);
            #[allow(clippy::manual_is_multiple_of)]
            if retry % 8 == 0 {
                tracing::warn!(
                    target: "anchors",
                    "检测到遗留 Anchor 锁，强制解锁后重算: {:?}, retry={}",
                    node.debug_info.get()._to_string(),
                    retry
                );
            }
            node.anchor_locked.set(false);
        }
        node.anchor_locked.set(true);

        // 可选调试：设置 `ANCHORS_RECALC_TRACE=1` 可打印正在重算的节点信息，便于跟踪依赖链。
        if recalc_trace_enabled() {
            println!("RECALC {:?}", node.debug_info.get()._to_string());
        }

        let trace_key = if lock_trace_enabled() {
            Some(node.key().raw_token())
        } else {
            None
        };
        if let Some(key) = trace_key {
            lock_trace_store(key, node.debug_info.get()._to_string());
        }

        let _lock_guard = AnchorLockGuard {
            trace_key,
            node_key: node.key(),
            graph,
        };

        // ┌─────────────────────────────────────────────────────────────┐
        // │ 兜底保护：节点可能在入队后被回收（handle_count 归零），       │
        // │ 此时 anchor 已被置空，继续重算只会 panic；直接跳过即可。       │
        // └─────────────────────────────────────────────────────────────┘
        if unsafe { (*this_anchor.get()).is_none() } {
            if cfg!(debug_assertions) {
                // 若 anchor 为空但持有者计数大于 0，说明释放链路异常，应当尽快排查。
                debug_assert!(
                    node.handle_count() == 0,
                    "recalculate: anchor=None 但 handle_count>0，疑似非法释放路径，token={:?} debug={}",
                    node.key().raw_token(),
                    node.debug_info.get()._to_string()
                );
            }
            return true;
        }

        let anchor_ref = unsafe {
            (*this_anchor.get())
                .as_mut()
                .expect("anchor 已被移除，无法重算")
        };
        {
            // 把之前因借用冲突积累的 dirty 消息在这里一次性补偿。
            let mut pending = node.pending_dirty.borrow_mut();
            for child in pending.drain(..) {
                anchor_ref.dirty(&child);
            }
        }

        let poll_result = anchor_ref.poll_updated(&mut ecx);
        node.recalc_retry.set(0);
        let pending_on_anchor_get = ecx.pending_on_anchor_get;
        let invalid_token_requested = ecx.invalid_token_requested;
        let handle_waiting = |poll_debug: Poll| {
            if emg_debug_env::bool_lenient("ANCHORS_DEBUG_PENDING") {
                println!(
                    "PENDING node={:?} pending_on_anchor_get={}",
                    node.debug_info.get()._to_string(),
                    pending_on_anchor_get
                );
            }
            if pending_on_anchor_get {
                // looks like we requested an anchor that isn't yet calculated, so we
                // reinsert into the graph directly; our height either was higher than this
                // requested anchor's already, or it was updated so it's higher now.
                return false;
            }

            // ──────────────────────────────────────────────────────────────
            // 意外 Pending：
            // - `poll_updated` 返回 Pending，但本次 poll 没有 request 任何“未 Ready 的子 Anchor”。
            // - 这意味着它在等待“非 anchors 依赖”（比如外部 Future/通道/时钟），
            //   anchors 无法追踪依赖关系，继续 requeue 会导致 stabilize 自旋。
            //
            // 处理策略：
            // - 若该节点曾经 Ready 过（有旧输出）：冻结旧输出并记录告警，避免 GUI 崩溃；
            // - 若从未 Ready（无旧输出）：无法提供降级输出，仍然 panic，且附带 node 信息。
            // ──────────────────────────────────────────────────────────────
            let raw_token = node.key().raw_token();
            let count = self.bump_unexpected_pending_count(raw_token);
            if Self::should_log_unexpected_pending(count) {
                let debug = node.debug_info.get()._to_string();
                eprintln!(
                    "[anchors][unexpected_pending] node={debug} token={raw_token} seen={count} poll={poll_debug:?}"
                );
                // NOTE: 只在第一次采样时抓一次回溯，避免高频路径产生大量 backtrace 分配。
                if count == 1 {
                    let bt = Backtrace::force_capture();
                    eprintln!("[anchors][unexpected_pending] backtrace:\n{bt}");
                }
            }

            if node.last_ready.get().is_some() {
                // 冻结：把本轮视为“已 ready”，避免不断 requeue。
                node.last_ready.set(Some(self.generation));
                return true;
            }

            panic!(
                "poll_updated 返回 Pending，但未 request 任何未就绪子 Anchor，且节点无历史输出无法降级；node={} token={raw_token}",
                node.debug_info.get()._to_string(),
            );
        };
        match poll_result {
            Poll::PendingInvalidToken => {
                // ──────────────────────────────────────────────────────────────
                // 失效 token 兜底：
                // - invalid token 并非“子节点尚未计算完成”，而是“子节点已被回收”。
                // - 若继续按 Pending 逻辑重排队，会导致 stabilize 打转 + 日志刷屏。
                // - 因此：
                //   - 若该节点曾经 Ready 过（有旧输出），则保持旧输出并标记为 Ready，避免死循环；
                //   - 若该节点从未 Ready（无旧输出），无法降级，直接 panic 以便定位根因。
                // ──────────────────────────────────────────────────────────────
                debug_assert!(
                    invalid_token_requested || !cfg!(debug_assertions),
                    "poll_updated 返回 PendingInvalidToken，但 ecx.invalid_token_requested=false，node={}",
                    node.debug_info.get()._to_string()
                );
                if node.last_ready.get().is_some() {
                    node.last_ready.set(Some(self.generation));
                    return true;
                }
                panic!(
                    "slotmap: 检测到失效 token request，且节点尚无历史输出无法降级；请开启 ANCHORS_INVALID_TOKEN_BACKTRACE=1 定位根因，node={}",
                    node.debug_info.get()._to_string()
                );
            }
            poll @ Poll::Pending => handle_waiting(poll),
            #[cfg(feature = "anchors_pending_queue")]
            poll @ Poll::PendingDefer => handle_waiting(poll),
            Poll::Updated => {
                if emg_debug_env::bool_lenient("ANCHORS_DEBUG_PARENTS") {
                    let parents = node.clean_parents();
                    println!(
                        "UPDATED {:?} parents={}",
                        node.debug_info.get()._to_string(),
                        parents.len()
                    );
                }
                // make sure all parents are marked as dirty, and observed parents are recalculated
                mark_dirty(graph, node, true);
                node.last_update.set(Some(self.generation));
                node.last_ready.set(Some(self.generation));
                true
            }
            Poll::Unchanged => {
                node.last_ready.set(Some(self.generation));
                true
            }
        }
    }

    /// Returns a debug string containing the current state of the recomputation graph.
    pub fn debug_state(&self) -> String {
        // for (node_id, _) in nodes.iter() {
        //     let node = self.graph.get(node_id).unwrap();
        //     let necessary = if self.graph.is_necessary(node_id) {
        //         "necessary"
        //     } else {
        //         "   --    "
        //     };
        //     let observed = if Self::check_observed_raw(node) == ObservedState::Observed {
        //         "observed"
        //     } else {
        //         "   --   "
        //     };
        //     let state = match self.to_recalculate.borrow_mut().state(node_id) {
        //         RecalcState::NeedsRecalc => "NeedsRecalc  ",
        //         RecalcState::PendingRecalc => "PendingRecalc",
        //         RecalcState::Ready => "Ready        ",
        //     };
        //     debug += &format!(
        //         "{:>80}  {}  {}  {}\n",
        //         node.debug_info.get().to_string(),
        //         necessary,
        //         observed,
        //         state
        //     );
        // }
        // debug
        "".to_string()
    }

    /// 导出指定若干 AnchorToken 作为根的依赖图，生成 GraphViz DOT 文本
    ///
    /// 设计说明：
    /// - 仅遍历从 roots 出发可达的子图，避免整个图的暴力扫描（Graph2 不暴露全量迭代接口）。
    /// - 边分两类：
    ///   - CleanParent 依赖：计算上的普通父依赖（使用实线）
    ///   - NecessaryChild 依赖：必要子链路（使用虚线）
    /// - 节点标签：优先使用 `AnchorDebugInfo` 的位置信息（文件:行:列 + 名称），否则使用类型名。
    ///
    /// 使用方式（示例）：
    /// ```ignore
    /// let roots = vec![anchor_a.token(), anchor_b.token()];
    /// let dot = engine.export_dot_from_tokens(&roots);
    /// println!("{}", dot);
    /// ```
    pub fn export_dot_from_tokens(&self, roots: &[AnchorToken]) -> String {
        // 使用 BFS 遍历从 roots 可达的子图
        let mut visited: IndexSet<AnchorToken> = IndexSet::new();
        let mut queue: VecDeque<AnchorToken> = VecDeque::new();

        for &r in roots {
            if visited.insert(r) {
                queue.push_back(r);
            }
        }

        // 收集节点与边
        #[derive(Clone, Copy)]
        enum EdgeKind {
            CleanParent,
            Necessary,
        }

        let mut edges: Vec<(AnchorToken, AnchorToken, EdgeKind)> = Vec::new();
        let mut labels: IndexMap<AnchorToken, String> = IndexMap::new();

        self.graph.with(|graph| {
            while let Some(tok) = queue.pop_front() {
                if let Some(node) = graph.get(tok) {
                    // 生成节点标签：优先使用 debug 位置信息
                    let dbg = node.debug_info.get();
                    let mut label = match dbg.location {
                        Some((name, loc)) => {
                            // 例如："file.rs:123:45\nname"
                            format!("{}\n{}", loc, name)
                        }
                        None => dbg._to_string(),
                    };
                    let slot_id = format!("{:?}", tok.ptr).replace('"', "\\\"");
                    let slot_token = node.slot_token.get();
                    let recalc = graph2::recalc_state(node);
                    label.push_str(&format!("\\nslot_id={slot_id}"));
                    label.push_str(&format!("\\ntoken={slot_token}"));
                    label.push_str(&format!("\\nrecalc={recalc:?}"));
                    labels.entry(tok).or_insert(label);

                    // Clean parents（从当前 -> 父）
                    for p in node.clean_parents() {
                        let ptok = p.key();
                        edges.push((tok, ptok, EdgeKind::CleanParent));
                        if visited.insert(ptok) {
                            queue.push_back(ptok);
                        }
                    }
                    // Necessary children（统一采用 依赖 -> 使用者 的方向：child -> self）
                    for c in node.necessary_children() {
                        let ctok = c.key();
                        edges.push((ctok, tok, EdgeKind::Necessary));
                        if visited.insert(ctok) {
                            queue.push_back(ctok);
                        }
                    }
                }
            }
        });

        // 生成 DOT 文本
        let mut out = String::new();
        out.push_str("digraph Anchors {\n");
        out.push_str("  rankdir=LR;\n");
        out.push_str("  node [shape=box, fontsize=10];\n");
        #[cfg(feature = "anchors_slotmap")]
        {
            let audit = self.token_audit_snapshot();
            let last = audit
                .last_deleted_token
                .map(|v| v.to_string())
                .unwrap_or_else(|| "None".to_string());
            out.push_str(&format!(
                "  labelloc=\"t\";\n  label=\"token_next={} | last_deleted={}\";\n",
                audit.next_token, last
            ));
        }

        // 打印节点
        for (tok, label) in labels.iter() {
            // 使用 token.ptr 的地址作为稳定 id（Debug 格式包含指针值）
            out.push_str(&format!(
                "  \"{:?}\" [label=\"{}\"];\n",
                tok.ptr,
                label.replace('\"', "\\\"")
            ));
        }

        // 打印边
        for (a, b, kind) in edges.iter().copied() {
            let style = match kind {
                EdgeKind::CleanParent => "solid",
                EdgeKind::Necessary => "dashed",
            };
            out.push_str(&format!(
                "  \"{:?}\" -> \"{:?}\" [style={}];\n",
                a.ptr, b.ptr, style
            ));
        }

        out.push_str("}\n");
        out
    }

    /// 与 `export_dot_from_tokens` 类似，但允许用户提供 `names` 来覆盖节点标签（用于显示变量名等）。
    pub fn export_dot_from_tokens_with_names(
        &self,
        roots: &[AnchorToken],
        names: &IndexMap<AnchorToken, String>,
    ) -> String {
        let mut visited: IndexSet<AnchorToken> = IndexSet::new();
        let mut queue: VecDeque<AnchorToken> = VecDeque::new();
        for &r in roots {
            if visited.insert(r) {
                queue.push_back(r);
            }
        }

        #[derive(Clone, Copy)]
        enum EdgeKind {
            CleanParent,
            Necessary,
        }

        let mut edges: Vec<(AnchorToken, AnchorToken, EdgeKind)> = Vec::new();
        let mut labels: IndexMap<AnchorToken, String> = IndexMap::new();

        self.graph.with(|graph| {
            while let Some(tok) = queue.pop_front() {
                if let Some(node) = graph.get(tok) {
                    let dbg = node.debug_info.get();
                    let default_label = match dbg.location {
                        Some((name, loc)) => format!("{}\n{}", loc, name),
                        None => dbg._to_string(),
                    };
                    let mut label = names.get(&tok).cloned().unwrap_or(default_label);
                    let slot_id = format!("{:?}", tok.ptr).replace('"', "\\\"");
                    let slot_token = node.slot_token.get();
                    let recalc = graph2::recalc_state(node);
                    label.push_str(&format!("\\nslot_id={slot_id}"));
                    label.push_str(&format!("\\ntoken={slot_token}"));
                    label.push_str(&format!("\\nrecalc={recalc:?}"));
                    labels.entry(tok).or_insert(label);

                    for p in node.clean_parents() {
                        let ptok = p.key();
                        edges.push((tok, ptok, EdgeKind::CleanParent));
                        if visited.insert(ptok) {
                            queue.push_back(ptok);
                        }
                    }
                    for c in node.necessary_children() {
                        let ctok = c.key();
                        edges.push((ctok, tok, EdgeKind::Necessary));
                        if visited.insert(ctok) {
                            queue.push_back(ctok);
                        }
                    }
                }
            }
        });

        let mut out = String::new();
        out.push_str("digraph Anchors {\n");
        out.push_str("  rankdir=LR;\n");
        out.push_str("  node [shape=box, fontsize=10];\n");
        for (tok, label) in labels.iter() {
            out.push_str(&format!(
                "  \"{:?}\" [label=\"{}\"];\n",
                tok.ptr,
                label.replace('\"', "\\\"")
            ));
        }
        for (a, b, kind) in edges.iter().copied() {
            let style = match kind {
                EdgeKind::CleanParent => "solid",
                EdgeKind::Necessary => "dashed",
            };
            out.push_str(&format!(
                "  \"{:?}\" -> \"{:?}\" [style={}];\n",
                a.ptr, b.ptr, style
            ));
        }
        out.push_str("}\n");
        out
    }

    pub fn check_observed<T>(&self, anchor: &Anchor<T>) -> ObservedState {
        self.graph.with(|graph| {
            let node = self.expect_node(&graph, anchor.token(), "check_observed");
            Self::check_observed_raw(node)
        })
    }

    /// Returns whether an Anchor is Observed, Necessary, or Unnecessary.
    pub fn check_observed_raw(node: NodeGuard<'_>) -> ObservedState {
        if node.observed.get() {
            return ObservedState::Observed;
        }
        if node.necessary_count.get() > 0 {
            ObservedState::Necessary
        } else {
            ObservedState::Unnecessary
        }
    }
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Debug 开关（热路径）缓存
//
// 背景：
// - `mark_dirty/request_node/recalculate` 都是“每个节点每次 stabilize 都会走”的热路径。
// - 若在这些路径里直接 `bool_lenient("XXX")`，会产生大量字符串查找/比较开销。
//
// 策略：
// - 在 debug_assertions 下才允许通过 env 打印调试信息。
// - release/bench 下直接返回 false，保证零运行时开销。
// ─────────────────────────────────────────────────────────────────────────────
#[inline]
fn debug_mark_enabled() -> bool {
    #[cfg(debug_assertions)]
    {
        static ENABLED: OnceLock<bool> = OnceLock::new();
        *ENABLED.get_or_init(|| emg_debug_env::bool_lenient("ANCHORS_DEBUG_MARK"))
    }

    #[cfg(not(debug_assertions))]
    {
        false
    }
}

#[inline]
fn debug_dirty_flow_enabled() -> bool {
    #[cfg(debug_assertions)]
    {
        static ENABLED: OnceLock<bool> = OnceLock::new();
        *ENABLED.get_or_init(|| emg_debug_env::bool_lenient("ANCHORS_DEBUG_DIRTY_FLOW"))
    }

    #[cfg(not(debug_assertions))]
    {
        false
    }
}

#[inline]
fn debug_request_enabled() -> bool {
    #[cfg(debug_assertions)]
    {
        static ENABLED: OnceLock<bool> = OnceLock::new();
        *ENABLED.get_or_init(|| emg_debug_env::bool_lenient("ANCHORS_DEBUG_REQUEST"))
    }

    #[cfg(not(debug_assertions))]
    {
        false
    }
}

#[inline]
fn debug_requeue_enabled() -> bool {
    #[cfg(debug_assertions)]
    {
        static ENABLED: OnceLock<bool> = OnceLock::new();
        *ENABLED.get_or_init(|| emg_debug_env::bool_lenient("ANCHORS_DEBUG_REQUEUE"))
    }

    #[cfg(not(debug_assertions))]
    {
        false
    }
}

#[inline]
fn recalc_trace_enabled() -> bool {
    #[cfg(debug_assertions)]
    {
        static ENABLED: OnceLock<bool> = OnceLock::new();
        *ENABLED.get_or_init(|| emg_debug_env::bool_lenient("ANCHORS_RECALC_TRACE"))
    }

    #[cfg(not(debug_assertions))]
    {
        false
    }
}

// 全局可用的锁 trace 开关，供 mark_dirty 等非 recalc 路径复用。
fn lock_trace_enabled() -> bool {
    static TRACE: OnceLock<bool> = OnceLock::new();
    *TRACE.get_or_init(|| emg_debug_env::bool_lenient("ANCHORS_LOCK_TRACE"))
}

/// ══════════════════════════════════════════════════════════════════════
/// STRICT 模式下的“重入锁”panic（冷路径）
///
/// 背景：
/// - `Engine::recalculate` 是极热函数；
/// - 但 “anchor_locked 且 strict=on” 属于极少发生的灾难性路径（应当尽快暴露根因）。
///
/// 目标：
/// - 把大段 format/backtrace/panic 逻辑挪到冷函数中，减少热路径的指令体积与 i-cache 压力；
/// - 行为保持不变：一旦命中 strict 重入，立刻 panic，并附带足够定位信息。
/// ══════════════════════════════════════════════════════════════════════
#[cold]
#[inline(never)]
fn panic_anchor_locked_strict(token: u64, node_debug: String, trace: Option<String>) -> ! {
    let trace = trace.unwrap_or_else(|| {
        "未记录 trace，请开启 ANCHORS_LOCK_TRACE 以获取更多定位信息".to_string()
    });
    let bt_now = Backtrace::force_capture();
    panic!(
        "slotmap: detected anchor_locked while strict mode enabled,疑似重入缺陷 token={token} node={node_debug}\n上次持锁位置: {trace}\n当前回溯:\n{bt_now:?}",
    );
}

// skip_self = true indicates output has *definitely* changed, but node has been recalculated
// skip_self = false indicates node has not yet been recalculated
fn mark_dirty<'a>(graph: Graph2Guard<'a>, node: NodeGuard<'a>, skip_self: bool) {
    #[cfg(debug_assertions)]
    trace!("mark_dirty {:?} ", node.debug_info.get(),);
    if skip_self {
        if debug_mark_enabled() {
            let parents_len = node.parents_len();
            println!(
                "mark_dirty skip_self node={:?} parents={}",
                node.debug_info.get()._to_string(),
                parents_len
            );
        }
        let child = node.key();
        node.drain_parents(|parent| {
            if parent.anchor_locked.get() {
                // 父节点正在重算，记录一次补偿重算请求，解锁后再统一入队，避免 strict 模式下的重入 panic。
                push_pending_dirty(parent, child);
                parent.pending_recalc.set(true);
                if debug_dirty_flow_enabled() {
                    println!(
                        "DIRTY flow skip queue (parent locked) child={:?} parent={:?}",
                        node.debug_info.get()._to_string(),
                        parent.debug_info.get()._to_string()
                    );
                }
                if lock_trace_enabled() {
                    tracing::warn!(
                        target: "anchors",
                        "skip queue_recalc: parent locked; child={:?} parent={:?}",
                        node.debug_info.get()._to_string(),
                        parent.debug_info.get()._to_string()
                    );
                }
                // 直接补偿入队一次：如果当下锁未释放，队列弹出时会跳过，等解锁后可正常重算。
                graph.queue_recalc_force(parent);
                return;
            }

            // 常规路径：父节点未锁定时，直接下发 dirty，避免写入 pending_dirty 再补偿。
            if let Some(anchor_impl) = unsafe { (*parent.anchor.get()).as_mut() } {
                anchor_impl.dirty(&child);
            }

            graph.queue_recalc(parent);
            mark_dirty0(graph, parent);
        });
    } else {
        mark_dirty0(graph, node);
    }
}

fn push_pending_dirty(parent: NodeGuard<'_>, child: NodeKey) {
    let mut pending = parent.pending_dirty.borrow_mut();
    if !pending.contains(&child) {
        pending.push(child);
        if debug_dirty_flow_enabled() {
            println!(
                "PENDING_DIRTY child={:?} ({}) -> parent={:?} ({})",
                child.raw_token(),
                unsafe { child.ptr.lookup_unchecked() }
                    .debug_info
                    .get()
                    ._to_string(),
                parent.key().raw_token(),
                parent.debug_info.get()._to_string()
            );
        }
    }
}

fn mark_dirty0<'a>(graph: Graph2Guard<'a>, next: NodeGuard<'a>) {
    #[cfg(debug_assertions)]
    trace!("mark_dirty0 {:?}", next);
    let id = next.key();
    if Engine::check_observed_raw(next) != ObservedState::Unnecessary {
        // 如果节点当前正处于重算（anchor_locked=true），说明父链路已在调用栈上，无需再次入队以免触发重入检测。
        if !next.anchor_locked.get() {
            graph.queue_recalc(next);
        }
    } else if graph2::recalc_state(next) == RecalcState::Ready {
        graph2::needs_recalc(next);
        if debug_mark_enabled() {
            println!(
                "mark_dirty0 unnecessary node={:?} parents={}",
                next.debug_info.get()._to_string(),
                next.parents_len()
            );
        }
        next.for_each_parent(|parent| {
            if parent.anchor_locked.get() {
                // 父节点正在重算：只记录 dirty，等待解锁后补偿入队。
                push_pending_dirty(parent, id);
                parent.pending_recalc.set(true);
                graph.queue_recalc_force(parent);
                return;
            }

            // 常规路径：直接下发 dirty，避免写入 pending_dirty 再补偿。
            if let Some(anchor_impl) = unsafe { (*parent.anchor.get()).as_mut() } {
                anchor_impl.dirty(&id);
            }

            graph.queue_recalc(parent);
            mark_dirty0(graph, parent);
        });
    }
}

/// Singlethread's implementation of Anchors' `DirtyHandle`, which allows a node with non-Anchors inputs to manually mark itself as dirty.
#[derive(Debug, Clone)]
pub struct DirtyHandle {
    num: NodeKey,
    dirty_marks: Rc<RefCell<Vec<NodeKey>>>,
}
impl crate::expert::DirtyHandle for DirtyHandle {
    fn mark_dirty(&self) {
        self.dirty_marks.borrow_mut().push(self.num);
    }
}

pub struct EngineContext<'eng> {
    engine: &'eng Engine,
}

pub struct EngineContextMut<'eng, 'gg> {
    engine: &'eng Engine,
    graph: Graph2Guard<'gg>,
    node: NodeGuard<'gg>,
    pending_on_anchor_get: bool,
    invalid_token_requested: bool,
}

impl<'eng> OutputContext<'eng> for EngineContext<'eng> {
    type Engine = Engine;

    fn get<'out, O: 'static>(&self, anchor: &Anchor<O>) -> &'out O
    where
        'eng: 'out,
    {
        self.engine.graph.with(|graph| {
            let node = self
                .engine
                .expect_node(&graph, anchor.token(), "EngineContext::get");
            if graph2::recalc_state(node) != RecalcState::Ready {
                panic!("attempted to get node that was not previously requested")
            }
            let anchor_impl = unsafe { (*node.anchor.get()).as_ref().unwrap() };
            let output: &O = anchor_impl
                .output(&mut EngineContext {
                    engine: self.engine,
                })
                .downcast_ref::<O>()
                .unwrap();
            output
        })
    }
}

impl<'eng, 'gg> UpdateContext for EngineContextMut<'eng, 'gg> {
    type Engine = Engine;

    fn get<'out, 'slf, O: 'static>(&'slf self, anchor: &Anchor<O>) -> &'out O
    where
        'slf: 'out,
    {
        self.engine.graph.with(|graph| {
            let node = self
                .engine
                .expect_node(&graph, anchor.token(), "EngineContextMut::get");
            if graph2::recalc_state(node) != RecalcState::Ready {
                panic!("attempted to get node that was not previously requested")
            }

            let anchor_impl = unsafe { (*node.anchor.get()).as_ref().unwrap() };
            let output: &O = anchor_impl
                .output(&mut EngineContext {
                    engine: self.engine,
                })
                .downcast_ref::<O>()
                .unwrap();
            output
        })
    }

    #[track_caller]
    fn request<'out, O: 'static>(&mut self, anchor: &Anchor<O>, necessary: bool) -> Poll {
        let token = anchor.token();
        let Some(child) = self.graph.get(token) else {
            // ──────────────────────────────────────────────────────────────
            // 兜底处理：
            // - 当 token 已失效/节点被回收时，不再 panic，避免 UI 直接崩溃。
            // - 该场景仍应视为异常，使用 warn 记录上下文，便于后续定位。
            // ──────────────────────────────────────────────────────────────
            self.pending_on_anchor_get = true;
            self.invalid_token_requested = true;

            let raw_token = token.raw_token();
            let count = self.engine.bump_invalid_token_count(raw_token);
            if Engine::should_log_invalid_token(count) {
                let audit = self.engine.token_audit_snapshot();
                let caller = Location::caller();
                let output_type = std::any::type_name::<O>();
                // ──────────────────────────────────────────────────────────────
                // 重要：在大多数示例里没有初始化 tracing subscriber，
                // 仅靠 tracing::warn! 会导致关键诊断信息“静默丢失”。
                //
                // 因此：当用户显式开启 `ANCHORS_INVALID_TOKEN_BACKTRACE=1` 时，
                // 这里额外用 stderr 打印一次，保证能看到“是谁请求了哪个失效 token”。
                // ──────────────────────────────────────────────────────────────
                if Engine::invalid_token_backtrace_enabled() {
                    // 尝试补充更多上下文：当前 ptr 所指向槽位的 token/debug/状态，
                    // 用来区分“token 代际不匹配（槽位已被复用）” vs “anchor 已被置空”。
                    let slot_debug = unsafe { token.ptr.lookup_unchecked() };
                    let slot_token = slot_debug.slot_token.get();
                    let slot_anchor_none = unsafe { (&*slot_debug.anchor.get()).is_none() };
                    let slot_anchor_locked = slot_debug.anchor_locked.get();
                    let slot_dbg_info = slot_debug.debug_info.get()._to_string();

                    eprintln!(
                        "[anchors][invalid_token][request] parent_token={} parent={} child_token={} output_type={} necessary={} caller={}:{}:{} audit_next_token={} audit_last_deleted={:?} seen={} slot_token={} slot_anchor_none={} slot_anchor_locked={} slot_debug={}",
                        self.node.key().raw_token(),
                        self.node.debug_info.get()._to_string(),
                        raw_token,
                        output_type,
                        necessary,
                        caller.file(),
                        caller.line(),
                        caller.column(),
                        audit.next_token,
                        audit.last_deleted_token,
                        count,
                        slot_token,
                        slot_anchor_none,
                        slot_anchor_locked,
                        slot_dbg_info
                    );
                }
                if count == 1 && Engine::invalid_token_backtrace_enabled() {
                    let bt = Backtrace::force_capture();
                    // stderr 再补一份 backtrace，避免 tracing subscriber 未初始化时丢信息。
                    eprintln!("[anchors][invalid_token][request] backtrace:\n{bt}");
                    tracing::warn!(
                        target: "anchors",
                        op = "request",
                        parent = %self.node.debug_info.get()._to_string(),
                        parent_token = self.node.key().raw_token(),
                        child_token = raw_token,
                        output_type,
                        necessary,
                        seen = count,
                        caller_file = caller.file(),
                        caller_line = caller.line(),
                        caller_column = caller.column(),
                        audit_next_token = audit.next_token,
                        audit_last_deleted = ?audit.last_deleted_token,
                        backtrace = %bt,
                        "EngineContextMut::request 发现失效 token（节点可能已被 GC/free），将保持旧输出并抑制重复日志"
                    );
                } else {
                    tracing::warn!(
                        target: "anchors",
                        op = "request",
                        parent = %self.node.debug_info.get()._to_string(),
                        parent_token = self.node.key().raw_token(),
                        child_token = raw_token,
                        output_type,
                        necessary,
                        seen = count,
                        caller_file = caller.file(),
                        caller_line = caller.line(),
                        caller_column = caller.column(),
                        audit_next_token = audit.next_token,
                        audit_last_deleted = ?audit.last_deleted_token,
                        "EngineContextMut::request 发现失效 token（节点可能已被 GC/free），将保持旧输出并抑制重复日志"
                    );
                }
            }

            // NOTE:
            // - 这里返回 PendingInvalidToken，用于区分：
            //   - “依赖尚未 ready”（Pending/PendingDefer）
            //   - “依赖已被 GC/free（token 失效）”（PendingInvalidToken）
            // - 上层 AnchorInner（例如 map_mut）可以据此选择保留旧输出降级，避免 stabilize 自旋或 panic。
            return Poll::PendingInvalidToken;
        };
        self.request_node(child, necessary)
    }

    #[track_caller]
    fn unrequest<'out, O: 'static>(&mut self, anchor: &Anchor<O>) {
        let token = anchor.token();
        let Some(child) = self.graph.get(token) else {
            let raw_token = token.raw_token();
            let count = self.engine.bump_invalid_token_count(raw_token);
            if Engine::should_log_invalid_token(count) {
                let caller = Location::caller();
                let output_type = std::any::type_name::<O>();
                tracing::warn!(
                    target: "anchors",
                    op = "unrequest",
                    parent = %self.node.debug_info.get()._to_string(),
                    parent_token = self.node.key().raw_token(),
                    child_token = raw_token,
                    output_type,
                    seen = count,
                    caller_file = caller.file(),
                    caller_line = caller.line(),
                    caller_column = caller.column(),
                    "EngineContextMut::unrequest 发现失效 token，已跳过（日志已去重）"
                );
            }
            return;
        };
        self.node.remove_necessary_child(child);
        Engine::update_necessary_children(child);
    }

    fn dirty_handle(&mut self) -> DirtyHandle {
        DirtyHandle {
            num: self.node.key(),
            dirty_marks: self.engine.dirty_marks.clone(),
        }
    }
}

impl<'eng, 'gg> EngineContextMut<'eng, 'gg> {
    fn request_node(&mut self, child: NodeGuard<'gg>, necessary: bool) -> Poll {
        let height_already_increased = match graph2::ensure_height_increases(child, self.node) {
            Ok(v) => v,
            Err(()) => {
                tracing::error!(
                    target: "anchors",
                    "检测到依赖环，跳过本次 request，parent={:?} child={:?}",
                    self.node.debug_info.get()._to_string(),
                    child.debug_info.get()._to_string()
                );
                return Poll::Unchanged;
            }
        };

        if debug_request_enabled() {
            println!(
                "REQUEST parent_token={} parent={:?} child_token={} child={:?} ready={:?} height_ok={}",
                self.node.key().raw_token(),
                self.node.debug_info.get()._to_string(),
                child.key().raw_token(),
                child.debug_info.get()._to_string(),
                graph2::recalc_state(child),
                height_already_increased
            );
        }

        let self_is_necessary = Engine::check_observed_raw(self.node) != ObservedState::Unnecessary;

        if graph2::recalc_state(child) != RecalcState::Ready {
            child.add_clean_parent(self.node);
            self.pending_on_anchor_get = true;
            self.graph.queue_recalc(child);
            if debug_request_enabled() {
                println!(
                    "REQUEST pending parent_token={} parent={:?} child_token={} child={:?} state={:?}",
                    self.node.key().raw_token(),
                    self.node.debug_info.get()._to_string(),
                    child.key().raw_token(),
                    child.debug_info.get()._to_string(),
                    graph2::recalc_state(child),
                );
            }
            if necessary && self_is_necessary {
                self.node.add_necessary_child(child);
            }
            self.defer_or_pending(child, necessary, self_is_necessary)
        } else if !height_already_increased {
            child.add_clean_parent(self.node);
            self.pending_on_anchor_get = true;
            self.defer_or_pending(child, necessary, self_is_necessary)
        } else {
            child.add_clean_parent(self.node);
            if necessary && self_is_necessary {
                self.node.add_necessary_child(child);
            }
            match (child.last_update.get(), self.node.last_ready.get()) {
                (Some(a), Some(b)) if a < b => Poll::Unchanged,
                (Some(a), Some(b)) if a == b && b == self.engine.generation => Poll::Updated,
                (Some(a), Some(b)) if a == b => Poll::Unchanged,
                (Some(_), Some(_)) => Poll::Updated,
                _ => Poll::Updated,
            }
        }
    }

    fn defer_or_pending(
        &mut self,
        child: NodeGuard<'gg>,
        necessary: bool,
        self_is_necessary: bool,
    ) -> Poll {
        let priority = PendingPriority::default_for(necessary, self_is_necessary);
        if self
            .engine
            .enqueue_pending_keys(self.node.key(), child.key(), necessary, priority)
        {
            #[cfg(feature = "anchors_pending_queue")]
            return Poll::PendingDefer;
            #[cfg(not(feature = "anchors_pending_queue"))]
            return Poll::Pending;
        }
        Poll::Pending
    }
}

pub trait GenericAnchor {
    fn dirty(&mut self, child: &NodeKey);
    fn poll_updated(&mut self, ctx: &mut EngineContextMut<'_, '_>) -> Poll;
    fn output<'slf, 'out>(&'slf self, ctx: &mut EngineContext<'out>) -> &'out dyn Any
    where
        'slf: 'out;
    fn debug_info(&self) -> AnchorDebugInfo;
}
impl<I: AnchorInner<Engine> + 'static> GenericAnchor for I {
    fn dirty(&mut self, child: &NodeKey) {
        AnchorInner::dirty(self, child)
    }
    fn poll_updated(&mut self, ctx: &mut EngineContextMut<'_, '_>) -> Poll {
        AnchorInner::poll_updated(self, ctx)
    }
    fn output<'slf, 'out>(&'slf self, ctx: &mut EngineContext<'out>) -> &'out dyn Any
    where
        'slf: 'out,
    {
        AnchorInner::output(self, ctx)
    }
    fn debug_info(&self) -> AnchorDebugInfo {
        let ad = AnchorDebugInfo {
            location: self.debug_location(),
            type_info: std::any::type_name::<I>(),
        };
        #[cfg(debug_assertions)]
        trace!("debug_info: {:?}", &ad._to_string());
        ad
    }
}

#[derive(Debug, Clone, Copy)]
pub struct AnchorDebugInfo {
    location: Option<(&'static str, &'static Location<'static>)>,
    type_info: &'static str,
}

impl AnchorDebugInfo {
    pub fn to_display(&self) -> String {
        match self.location {
            Some((name, location)) => format!("{} ({})", location, name),
            None => self.type_info.to_string(),
        }
    }
    // 兼容旧日志调用
    pub fn _to_string(&self) -> String {
        self.to_display()
    }
}
