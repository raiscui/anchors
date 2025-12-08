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
use tracing::trace;

pub use graph2::AnchorHandle;
#[cfg(feature = "anchors_slotmap")]
pub use graph2::GcStatsSnapshot;
pub use graph2::NodeKey as AnchorToken;

#[cfg(feature = "anchors_slotmap")]
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
#[cfg(feature = "anchors_slotmap")]
use emg_hasher::std::HashSet;
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
use std::cell::{Cell, RefCell};
#[cfg(feature = "anchors_slotmap")]
use std::collections::BTreeMap;
use std::collections::VecDeque;
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

    fn as_u8(self) -> u8 {
        self as u8
    }
}

#[cfg(feature = "anchors_slotmap")]
#[derive(Clone, Copy, Debug)]
struct PendingRequest {
    parent: NodeKey,
    child: NodeKey,
    necessary: bool,
    priority: PendingPriority,
}

#[cfg(feature = "anchors_slotmap")]
#[derive(Default)]
struct PendingQueue {
    buckets: BTreeMap<PendingPriority, IndexMap<NodeKey, PendingRequest>>,
    index: HashMap<NodeKey, PendingPriority>,
}

#[cfg(feature = "anchors_slotmap")]
#[derive(Default)]
struct PendingSubsetFilter {
    /// 需要立刻冲刷的节点 token 集，触发 subset stabilize 时仅处理这些节点。
    tokens: IndexSet<NodeKey>,
}

#[cfg(feature = "anchors_slotmap")]
impl PendingSubsetFilter {
    fn new(tokens: &[NodeKey]) -> Self {
        Self {
            tokens: tokens.iter().copied().collect(),
        }
    }

    fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }

    fn contains(&self, token: &NodeKey) -> bool {
        self.tokens.contains(token)
    }

    fn len(&self) -> usize {
        self.tokens.len()
    }

    fn sample_raw_tokens(&self, limit: usize) -> Vec<u64> {
        self.tokens
            .iter()
            .take(limit)
            .map(|token| token.raw_token())
            .collect()
    }
}

#[cfg(feature = "anchors_slotmap")]
type PendingSubsetArg<'a> = Option<&'a PendingSubsetFilter>;
#[cfg(not(feature = "anchors_slotmap"))]
type PendingSubsetArg<'a> = Option<&'a ()>;

#[cfg(feature = "anchors_slotmap")]
enum PendingEnqueueOutcome {
    Inserted,
    Replaced,
}

#[cfg(feature = "anchors_slotmap")]
struct PendingDrainOutcome {
    requests: Vec<PendingRequest>,
    subset_stats: Option<PendingSubsetDrainStats>,
}

#[cfg(feature = "anchors_slotmap")]
struct PendingSubsetDrainStats {
    requested_tokens: usize,
    drained_tokens: usize,
    unmatched_tokens: usize,
    remaining_after: usize,
    parent_match_hits: usize,
    sample_filter: Vec<NodeKey>,
    sample_drained: Vec<NodeKey>,
    sample_unmatched: Vec<NodeKey>,
    sample_remaining: Vec<NodeKey>,
    drained_priority_hist: BTreeMap<u8, usize>,
    remaining_priority_hist: BTreeMap<u8, usize>,
}

#[cfg(feature = "anchors_slotmap")]
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

    fn drain_all(&mut self) -> PendingDrainOutcome {
        let mut drained = Vec::with_capacity(self.len());
        while let Some(entry) = self.pop_front() {
            drained.push(entry);
        }
        PendingDrainOutcome {
            requests: drained,
            subset_stats: None,
        }
    }

    fn drain_subset(&mut self, filter: &PendingSubsetFilter) -> PendingDrainOutcome {
        if filter.is_empty() {
            return PendingDrainOutcome {
                requests: Vec::new(),
                subset_stats: None,
            };
        }
        if Self::debug_pending_queue_sample_enabled() && self.len() > 0 {
            // 仅采样前 20 条 pending 子节点，辅助定位未命中的种子链路
            const SAMPLE_LIMIT: usize = 20;
            let mut sample_tokens = Vec::with_capacity(SAMPLE_LIMIT);
            for bucket in self.buckets.values() {
                for token in bucket.keys().copied() {
                    sample_tokens.push(token);
                    if sample_tokens.len() >= SAMPLE_LIMIT {
                        break;
                    }
                }
                if sample_tokens.len() >= SAMPLE_LIMIT {
                    break;
                }
            }
            let sample_raw: Vec<u64> = sample_tokens.iter().map(NodeKey::raw_token).collect();
            tracing::info!(
                target: "anchors",
                log_event = "pending_subset_queue_sample",
                queue_len = self.len(),
                sample_raw = ?sample_raw,
                "pending queue sample before subset drain"
            );
        }
        let mut drained = Vec::new();
        let mut empty_priorities = Vec::new();
        let mut parent_match_hits = 0usize;
        let mut allowed = filter.tokens.clone();
        // 采样规模：扩大到 64，便于 subset retry/backfill 捕获更多剩余/未命中 token
        const SAMPLE: usize = 64;
        for (&priority, bucket) in self.buckets.iter_mut().rev() {
            let mut pending_keys: Vec<NodeKey> = bucket.keys().copied().collect();
            let mut progressed = true;
            while progressed && !pending_keys.is_empty() {
                progressed = false;
                let mut remaining = Vec::with_capacity(pending_keys.len());
                for token in pending_keys {
                    let matches_child = allowed.contains(&token);
                    let matches_parent = if matches_child {
                        false
                    } else {
                        bucket
                            .get(&token)
                            .map(|req| allowed.contains(&req.parent))
                            .unwrap_or(false)
                    };
                    if !matches_child && !matches_parent {
                        remaining.push(token);
                        continue;
                    }
                    progressed = true;
                    let entry = bucket
                        .shift_remove(&token)
                        .expect("pending entry missing while draining subset");
                    if matches_parent {
                        parent_match_hits = parent_match_hits.saturating_add(1);
                    }
                    self.index.remove(&token);
                    allowed.insert(entry.child);
                    drained.push(entry);
                }
                pending_keys = remaining;
            }
            if bucket.is_empty() {
                empty_priorities.push(priority);
            }
        }
        for priority in empty_priorities {
            self.buckets.remove(&priority);
        }
        let requested_total = filter.len();
        let drained_set: HashSet<NodeKey> = drained.iter().map(|req| req.child).collect();
        let unmatched_total = filter
            .tokens
            .iter()
            .filter(|token| !drained_set.contains(token))
            .count();
        let mut drained_hist = BTreeMap::new();
        for req in &drained {
            *drained_hist.entry(req.priority.as_u8()).or_insert(0usize) += 1;
        }
        let mut remaining_hist = BTreeMap::new();
        for (priority, bucket) in self.buckets.iter() {
            if bucket.is_empty() {
                continue;
            }
            remaining_hist.insert(priority.as_u8(), bucket.len());
        }
        let sample_filter: Vec<NodeKey> = filter.tokens.iter().take(SAMPLE).copied().collect();
        let sample_drained: Vec<NodeKey> =
            drained.iter().take(SAMPLE).map(|req| req.child).collect();
        let sample_unmatched: Vec<NodeKey> = filter
            .tokens
            .iter()
            .filter(|token| !drained_set.contains(token))
            .take(SAMPLE)
            .copied()
            .collect();
        let sample_remaining: Vec<NodeKey> = self
            .buckets
            .values()
            .flat_map(|bucket| bucket.values().map(|req| req.child))
            .take(SAMPLE)
            .collect();
        let stats = PendingSubsetDrainStats {
            requested_tokens: requested_total,
            drained_tokens: drained.len(),
            unmatched_tokens: unmatched_total,
            remaining_after: self.len(),
            parent_match_hits,
            sample_filter,
            sample_drained,
            sample_unmatched,
            sample_remaining,
            drained_priority_hist: drained_hist,
            remaining_priority_hist: remaining_hist,
        };
        PendingDrainOutcome {
            requests: drained,
            subset_stats: Some(stats),
        }
    }

    #[cfg(feature = "anchors_slotmap")]
    fn debug_pending_queue_sample_enabled() -> bool {
        use std::sync::OnceLock;

        static ENABLED: OnceLock<bool> = OnceLock::new();
        *ENABLED.get_or_init(|| {
            std::env::var("ANCHORS_PENDING_DEBUG_SAMPLE")
                .map(|v| matches!(v.as_str(), "1" | "true" | "TRUE" | "on" | "ON"))
                .unwrap_or(false)
        })
    }
}

/// The main execution engine of Singlethread.
pub struct Engine {
    // TODO store Nodes on heap directly?? maybe try for Rc<RefCell<SlotMap>> now
    graph: Rc<Graph2>,
    dirty_marks: Rc<RefCell<Vec<NodeKey>>>,

    // tracks the current stabilization generation; incremented on every stabilize
    generation: Generation,
    #[cfg(feature = "anchors_slotmap")]
    pending_requests: Rc<RefCell<PendingQueue>>,
    #[cfg(feature = "anchors_slotmap")]
    pending_stats: Rc<PendingStatsInner>,
    #[cfg(feature = "anchors_slotmap")]
    last_subset_remaining: Rc<RefCell<Vec<NodeKey>>>,
    #[cfg(feature = "anchors_slotmap")]
    last_subset_remaining_debug: Rc<RefCell<Vec<String>>>,
}

struct Mounter {
    graph: Rc<Graph2>,
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
}

impl Engine {
    #[cfg(feature = "anchors_slotmap")]
    fn debug_pending_enabled() -> bool {
        static ENABLED: OnceLock<bool> = OnceLock::new();
        *ENABLED.get_or_init(|| {
            std::env::var("ANCHORS_DEBUG_PENDING")
                .map(|v| v != "0")
                .unwrap_or(false)
        })
    }

    #[cfg(not(feature = "anchors_slotmap"))]
    #[inline]
    fn debug_pending_enabled() -> bool {
        false
    }

    #[cfg(feature = "anchors_slotmap")]
    fn defer_disabled() -> bool {
        static DISABLED: OnceLock<bool> = OnceLock::new();
        *DISABLED.get_or_init(|| {
            std::env::var("ANCHORS_DEFER_DISABLED")
                .map(|v| v != "0")
                .unwrap_or(false)
        })
    }

    #[cfg(not(feature = "anchors_slotmap"))]
    #[inline]
    fn defer_disabled() -> bool {
        true
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
        let graph = Rc::new(Graph2::new(max_height));
        let mounter = Mounter {
            graph: graph.clone(),
        };
        DEFAULT_MOUNTER.with(|v| *v.borrow_mut() = Some(mounter));
        Self {
            graph,
            dirty_marks: Default::default(),
            generation: Generation::new(),
            #[cfg(feature = "anchors_slotmap")]
            pending_requests: Rc::new(RefCell::new(PendingQueue::default())),
            #[cfg(feature = "anchors_slotmap")]
            pending_stats: Rc::new(PendingStatsInner::default()),
            #[cfg(feature = "anchors_slotmap")]
            last_subset_remaining: Rc::new(RefCell::new(Vec::new())),
            #[cfg(feature = "anchors_slotmap")]
            last_subset_remaining_debug: Rc::new(RefCell::new(Vec::new())),
        }
    }

    /// 输出 pending 队列的聚合指标，便于在 demo/集成测试后记录基线。
    #[must_use]
    pub fn pending_stats_snapshot(&self) -> AnchorsPendingStats {
        #[cfg(feature = "anchors_slotmap")]
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

        #[cfg(not(feature = "anchors_slotmap"))]
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

    #[cfg(feature = "anchors_slotmap")]
    pub fn take_last_subset_remaining_samples(&self) -> (Vec<AnchorToken>, Vec<String>) {
        let tokens = {
            let mut slot = self.last_subset_remaining.borrow_mut();
            slot.drain(..).collect::<Vec<_>>()
        };
        let debugs = {
            let mut slot = self.last_subset_remaining_debug.borrow_mut();
            slot.drain(..).collect::<Vec<_>>()
        };
        (tokens, debugs)
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

    #[cfg(feature = "anchors_slotmap")]
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

    #[cfg(not(feature = "anchors_slotmap"))]
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

    /// Retrieves the value of an Anchor, recalculating dependencies as necessary to get the
    /// latest value.
    pub fn get<O: Clone + 'static>(&mut self, anchor: &Anchor<O>) -> O {
        #[cfg(feature = "anchors_slotmap")]
        if let Some(val) = self.fast_path_ready_value(anchor) {
            return val;
        }

        // 在读取前主动标记为 observed，确保父链路必要关系被建立，避免 clean_parents 被回收后无法向上脏传播。
        // self.mark_observed(anchor);
        self.stabilize();
        self.graph.with(|graph| {
            let anchor_node = self.ready_node(&graph, anchor.token(), "get::<O> 读取节点");
            self.read_ready_output(anchor_node, "get::<O> 读取节点")
        })
    }
    pub fn get_with<O: Clone + 'static, F: FnOnce(&O) -> R, R>(
        &mut self,
        anchor: &Anchor<O>,
        func: F,
    ) -> R {
        // self.mark_observed(anchor);
        // stabilize once before, since the stabilization process may mark our requested node
        // as dirty
        self.stabilize();
        self.graph.with(|graph| {
            let target_node =
                self.ready_node(&graph, anchor.token(), "get_with 读取 anchor/output");
            let borrowed = unsafe { &*target_node.anchor.get() };
            let o = borrowed
                .as_ref()
                .unwrap()
                .output(&mut EngineContext { engine: self })
                .downcast_ref::<O>()
                .unwrap();
            func(o)
        })
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

    pub(crate) fn update_dirty_marks(&mut self) {
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
        trace!("stabilize");
        self.update_dirty_marks();
        self.generation.increment();
        self.stabilize_with_pending_subset(None);
        trace!("....stabilize");
    }

    pub fn stabilize_subset(&mut self, tokens: &[NodeKey]) {
        #[cfg(feature = "anchors_slotmap")]
        {
            if tokens.is_empty() {
                return;
            }
            trace!("stabilize_subset");
            self.update_dirty_marks();
            self.generation.increment();
            let filter = PendingSubsetFilter::new(tokens);
            self.stabilize_with_pending_subset(Some(&filter));
            trace!("....stabilize_subset");
            return;
        }

        #[cfg(not(feature = "anchors_slotmap"))]
        {
            let _ = tokens;
            self.stabilize();
        }
    }

    #[cfg(feature = "anchors_slotmap")]
    pub fn collect_related_tokens(&self, roots: &[NodeKey], depth: usize) -> Vec<NodeKey> {
        if roots.is_empty() {
            return Vec::new();
        }
        if depth == 0 {
            return roots.to_vec();
        }

        let mut visited: IndexSet<NodeKey> = IndexSet::new();
        let mut queue: VecDeque<(NodeKey, usize)> = VecDeque::new();
        for &root in roots {
            if visited.insert(root) {
                queue.push_back((root, 0));
            }
        }

        self.graph.with(|graph| {
            while let Some((token, level)) = queue.pop_front() {
                if level >= depth {
                    continue;
                }
                if let Some(node) = graph.get(token) {
                    for parent in node.clean_parents() {
                        let key = parent.key();
                        if visited.insert(key) {
                            queue.push_back((key, level + 1));
                        }
                    }
                    for child in node.necessary_children() {
                        let key = child.key();
                        if visited.insert(key) {
                            queue.push_back((key, level + 1));
                        }
                    }
                }
            }
        });

        visited.into_iter().collect()
    }

    #[cfg(not(feature = "anchors_slotmap"))]
    pub fn collect_related_tokens(&self, roots: &[NodeKey], _depth: usize) -> Vec<NodeKey> {
        roots.to_vec()
    }

    /// internal function for stabilization. does not update dirty marks or increment the stabilization number
    fn stabilize0(&self) {
        self.stabilize_with_pending_subset(None);
    }

    fn stabilize_with_pending_subset(&self, pending_subset: PendingSubsetArg<'_>) {
        trace!("stabilize0");
        self.graph.with(|graph| {
            // 可选调试：检测重算队列是否异常“打转”。设置 `ANCHORS_DEBUG_SPIN=1` 时启用。
            let mut spin_counter: usize = 0;
            let spin_debug = std::env::var("ANCHORS_DEBUG_SPIN")
                .map(|v| v != "0")
                .unwrap_or(false);
            // 自适应上限：根据当前激活节点数放宽容忍度，避免正常的高度调整/依赖扩展触发误报。
            // 理念：有 N 个节点时，合法的“高度回填+新依赖建图”往往需要 O(N) 次出队；乘以 4 作为缓冲。
            let spin_limit = if spin_debug {
                #[cfg(feature = "anchors_slotmap")]
                {
                    let active_nodes = graph.active_nodes();
                    std::cmp::max(64, active_nodes.saturating_mul(4))
                }
                #[cfg(not(feature = "anchors_slotmap"))]
                {
                    // 非 slotmap 后端暂无 active_nodes 统计，使用固定阈值以维持基本保护。
                    64
                }
            } else {
                0
            };
            // let spin_limit = 13;//for test
            #[cfg(feature = "anchors_slotmap")]
            {
                graph.retry_pending_free();
            }
            while let Some((height, node)) = graph.recalc_pop_next() {
                if spin_debug {
                    spin_counter = spin_counter.saturating_add(1);
                    println!(
                        "STABILIZE_SPIN #{spin_counter} height={height} node={}",
                        node.debug_info.get()._to_string()
                    );
                    if spin_counter > spin_limit {
                        panic!(
                            "stabilize0 spin detected: 重算次数超过上限 {spin_limit}，最后节点={}",
                            node.debug_info.get()._to_string(),
                        );
                    }
                }
                let calculation_complete = if graph2::height(node) == height {
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
                if std::env::var("ANCHORS_ACTIVE_NODES_LOG").is_ok() {
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
        #[cfg(feature = "anchors_slotmap")]
        self.process_pending_requests(pending_subset);
        trace!("...stabilize0");
    }

    #[cfg(feature = "anchors_slotmap")]
    fn process_pending_requests(&self, pending_subset: PendingSubsetArg<'_>) {
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
            match pending_subset {
                Some(filter) => queue.drain_subset(filter),
                None => queue.drain_all(),
            }
        };
        let drained = outcome.requests;
        if let Some(stats) = outcome.subset_stats {
            self.log_pending_subset_stats(stats);
        }
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
    }

    #[cfg(feature = "anchors_slotmap")]
    fn log_pending_subset_stats(&self, stats: PendingSubsetDrainStats) {
        if !tracing::enabled!(tracing::Level::INFO) {
            return;
        }
        let sample_filter = Self::raw_tokens(&stats.sample_filter);
        let sample_drained = Self::raw_tokens(&stats.sample_drained);
        let sample_unmatched = Self::raw_tokens(&stats.sample_unmatched);
        let sample_remaining = Self::raw_tokens(&stats.sample_remaining);
        let sample_filter_debug = self.describe_tokens(&stats.sample_filter);
        let sample_unmatched_debug = self.describe_tokens(&stats.sample_unmatched);
        let sample_remaining_debug = self.describe_tokens(&stats.sample_remaining);
        {
            let mut slot = self.last_subset_remaining.borrow_mut();
            slot.clear();
            slot.extend(stats.sample_remaining.iter().copied());
        }
        {
            let mut slot = self.last_subset_remaining_debug.borrow_mut();
            slot.clear();
            slot.extend(sample_remaining_debug.iter().cloned());
        }
        tracing::info!(
            target: "anchors",
            log_event = "pending_subset_drain",
            requested_tokens = stats.requested_tokens,
            drained_tokens = stats.drained_tokens,
            unmatched_tokens = stats.unmatched_tokens,
            remaining_after = stats.remaining_after,
            parent_match_hits = stats.parent_match_hits,
            sample_filter = ?sample_filter,
            sample_filter_debug = ?sample_filter_debug,
            sample_drained = ?sample_drained,
            sample_unmatched = ?sample_unmatched,
            sample_unmatched_debug = ?sample_unmatched_debug,
            sample_remaining = ?sample_remaining,
            sample_remaining_debug = ?sample_remaining_debug,
            drained_priority_hist = ?stats.drained_priority_hist,
            remaining_priority_hist = ?stats.remaining_priority_hist,
            "pending subset drain stats"
        );
    }

    #[cfg(feature = "anchors_slotmap")]
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

    #[cfg(feature = "anchors_slotmap")]
    fn debug_pending_queue_sample_enabled() -> bool {
        use std::sync::OnceLock;

        static ENABLED: OnceLock<bool> = OnceLock::new();
        *ENABLED.get_or_init(|| {
            std::env::var("ANCHORS_PENDING_DEBUG_SAMPLE")
                .map(|v| matches!(v.as_str(), "1" | "true" | "TRUE" | "on" | "ON"))
                .unwrap_or(false)
        })
    }

    #[cfg(feature = "anchors_slotmap")]
    fn raw_tokens(tokens: &[NodeKey]) -> Vec<u64> {
        tokens.iter().map(|token| token.raw_token()).collect()
    }

    /// returns false if calculation is still pending
    fn recalculate<'a>(&self, graph: Graph2Guard<'a>, node: NodeGuard<'a>) -> bool {
        let this_anchor = &node.anchor;
        let mut ecx = EngineContextMut {
            engine: self,
            node,
            graph,
            pending_on_anchor_get: false,
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
                    std::env::var("ANCHORS_LOCK_STRICT")
                        .map(|v| v != "0")
                        .unwrap_or(true)
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
            *TRACE.get_or_init(|| {
                std::env::var("ANCHORS_LOCK_TRACE")
                    .map(|v| v != "0")
                    .unwrap_or(false)
            })
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
                    if std::env::var("ANCHORS_DEBUG_REQUEUE")
                        .map(|v| v != "0")
                        .unwrap_or(false)
                    {
                        println!(
                            "REQUEUE check node={:?} pending_recalc={}",
                            node.debug_info.get()._to_string(),
                            node.pending_recalc.get()
                        );
                    }
                    // 若锁期间收到“补偿重算”标记，解锁后再入队自身，既避免重入也不丢更新。
                    if node.pending_recalc.get() {
                        if std::env::var("ANCHORS_DEBUG_REQUEUE")
                            .map(|v| v != "0")
                            .unwrap_or(false)
                        {
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
                let bt_now = Backtrace::force_capture();
                panic!(
                    "slotmap: detected anchor_locked while strict mode enabled,疑似重入缺陷 {:?}\n上次持锁位置: {}\n当前回溯:\n{:?}",
                    node.debug_info.get()._to_string(),
                    trace.unwrap_or_else(|| {
                        "未记录 trace，请开启 ANCHORS_LOCK_TRACE 以获取更多定位信息".to_string()
                    }),
                    bt_now
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
        if std::env::var("ANCHORS_RECALC_TRACE")
            .map(|v| v != "0")
            .unwrap_or(false)
        {
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
        match poll_result {
            Poll::Pending | Poll::PendingDefer => {
                if std::env::var("ANCHORS_DEBUG_PENDING")
                    .map(|v| v != "0")
                    .unwrap_or(false)
                {
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
                    false
                } else {
                    // in the future, this means we polled on some non-anchors future. since
                    // that isn't supported for now, this just means something went wrong
                    panic!("poll_updated return pending without requesting another anchor");
                }
            }
            Poll::Updated => {
                if std::env::var("ANCHORS_DEBUG_PARENTS")
                    .map(|v| v != "0")
                    .unwrap_or(false)
                {
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

// 全局可用的锁 trace 开关，供 mark_dirty 等非 recalc 路径复用。
fn lock_trace_enabled() -> bool {
    static TRACE: OnceLock<bool> = OnceLock::new();
    *TRACE.get_or_init(|| {
        std::env::var("ANCHORS_LOCK_TRACE")
            .map(|v| v != "0")
            .unwrap_or(false)
    })
}

// skip_self = true indicates output has *definitely* changed, but node has been recalculated
// skip_self = false indicates node has not yet been recalculated
fn mark_dirty<'a>(graph: Graph2Guard<'a>, node: NodeGuard<'a>, skip_self: bool) {
    trace!("mark_dirty {:?} ", node.debug_info.get(),);
    if skip_self {
        if std::env::var("ANCHORS_DEBUG_MARK")
            .map(|v| v != "0")
            .unwrap_or(false)
        {
            let parents_len = node.parents_len();
            println!(
                "mark_dirty skip_self node={:?} parents={}",
                node.debug_info.get()._to_string(),
                parents_len
            );
        }
        node.drain_parents(|parent| {
            push_pending_dirty(parent, node.key());
            if parent.anchor_locked.get() {
                // 父节点正在重算，记录一次补偿重算请求，解锁后再统一入队，避免 strict 模式下的重入 panic。
                parent.pending_recalc.set(true);
                if std::env::var("ANCHORS_DEBUG_DIRTY_FLOW")
                    .map(|v| v != "0")
                    .unwrap_or(false)
                {
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
        if std::env::var("ANCHORS_DEBUG_DIRTY_FLOW")
            .map(|v| v != "0")
            .unwrap_or(false)
        {
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
    trace!("mark_dirty0 {:?}", next);
    let id = next.key();
    if Engine::check_observed_raw(next) != ObservedState::Unnecessary {
        // 如果节点当前正处于重算（anchor_locked=true），说明父链路已在调用栈上，无需再次入队以免触发重入检测。
        if !next.anchor_locked.get() {
            graph.queue_recalc(next);
        }
    } else if graph2::recalc_state(next) == RecalcState::Ready {
        graph2::needs_recalc(next);
        if std::env::var("ANCHORS_DEBUG_MARK")
            .map(|v| v != "0")
            .unwrap_or(false)
        {
            println!(
                "mark_dirty0 unnecessary node={:?} parents={}",
                next.debug_info.get()._to_string(),
                next.parents_len()
            );
        }
        next.for_each_parent(|parent| {
            push_pending_dirty(parent, id);
            if !parent.anchor_locked.get() {
                graph.queue_recalc(parent);
                mark_dirty0(graph, parent);
            }
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

    fn request<'out, O: 'static>(&mut self, anchor: &Anchor<O>, necessary: bool) -> Poll {
        let child =
            self.engine
                .expect_node(&self.graph, anchor.token(), "EngineContextMut::request");
        self.request_node(child, necessary)
    }

    fn unrequest<'out, O: 'static>(&mut self, anchor: &Anchor<O>) {
        let child =
            self.engine
                .expect_node(&self.graph, anchor.token(), "EngineContextMut::unrequest");
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

        if std::env::var("ANCHORS_DEBUG_REQUEST")
            .map(|v| v != "0")
            .unwrap_or(false)
        {
            println!(
                "REQUEST parent={:?} child={:?} ready={:?} height_ok={}",
                self.node.debug_info.get()._to_string(),
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
            if std::env::var("ANCHORS_DEBUG_REQUEST")
                .map(|v| v != "0")
                .unwrap_or(false)
            {
                println!(
                    "REQUEST pending child={:?} state={:?} parent={:?}",
                    child.debug_info.get()._to_string(),
                    graph2::recalc_state(child),
                    self.node.debug_info.get()._to_string()
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
                (Some(a), Some(b)) if a <= b => Poll::Unchanged,
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
            Poll::PendingDefer
        } else {
            Poll::Pending
        }
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
