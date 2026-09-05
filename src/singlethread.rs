//! Singlethread is Anchors' default execution engine. It's a single threaded engine capable of both
//! Adapton-style pull updates and — if `mark_observed` and `mark_unobserved` are used,
//! Incremental-style push updates.
//!
//! As of Semptember 2020, execution overhead per-node sits at around 100ns on this author's Macbook
//! Air, likely somewhat more if single node has a significant number of parents or children. Hopefully
//! this will significantly improve over the coming months.

mod anchor_pool;
mod generation;
// 诊断设施快照的 re-export：graph2 为私有模块，宿主经此路径读取计数。
mod graph2;

#[cfg(test)]
mod test;

use graph2::{Graph2, Graph2Guard, NodeGuard, NodeKey, RecalcState};
#[cfg(debug_assertions)]
use tracing::trace;

pub use anchor_pool::AnchorPoolStatsSnapshot;
pub use graph2::AnchorHandle;
pub use graph2::EpochGuard;
pub use graph2::GcStatsSnapshot;
pub use graph2::NodeKey as AnchorToken;

// ─────────────────────────────────────────────────────────────────────────────
// 失效 token 观测：用于在不崩溃的前提下，避免 warn 日志刷屏，同时保留“首个触发点”。
// - 只在 anchors_slotmap 下启用（token 单调 + free list 复用更常见）。
// - 以 child raw_token 作为 key 做去重与计数；同一个 token 一旦开始刷屏，通常就是同一根因。
// ─────────────────────────────────────────────────────────────────────────────
#[derive(Default)]
struct InvalidTokenLogInner {
    counts: HashMap<u64, u32>,
}

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
#[derive(Default)]
struct UnexpectedPendingLogInner {
    counts: HashMap<u64, u32>,
}

impl UnexpectedPendingLogInner {
    fn bump(&mut self, raw_token: u64) -> u32 {
        let entry = self.counts.entry(raw_token).or_insert(0);
        *entry = entry.saturating_add(1);
        *entry
    }
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

use emg_hasher::indexmap::{IndexMap, IndexSet};
use emg_hasher::std::HashMap;
use generation::Generation;
#[cfg(not(target_arch = "wasm32"))]
use libc::{RUSAGE_SELF, getrusage, rusage};
use std::any::Any;
use std::backtrace::Backtrace;
use std::cell::Cell;
use std::cell::RefCell;
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

/// The main execution engine of Singlethread.
pub struct Engine {
    // TODO store Nodes on heap directly?? maybe try for Rc<RefCell<SlotMap>> now
    graph: Rc<Graph2>,
    dirty_marks: Rc<RefCell<Vec<NodeKey>>>,
    ////////////////////////////////////////////////////////////////////////////////
    // dirty_marks_scratch: 复用 update_dirty_marks 的临时 Vec
    //
    // 背景:
    // - 旧实现用 `mem::take(dirty_marks)`:
    //   - dirty_marks 会被替换成全新 Vec(容量=0),
    //   - 本轮/下一轮再 push 时会反复触发 grow + alloc/free;
    // - 在“动态 anchor churn”场景下,这会放大为 `RawVecInner::finish_grow` 热点。
    //
    // 这里改为:
    // - 把本轮待处理 token drain 到 scratch(保留两边容量),
    // - 处理期间 dirty_marks 仍可继续接收新的 push(用于 stabilize 内部 set)。
    ////////////////////////////////////////////////////////////////////////////////
    dirty_marks_scratch: RefCell<Vec<NodeKey>>,

    // tracks the current stabilization generation; incremented on every stabilize
    generation: Generation,
    invalid_token_log: Rc<RefCell<InvalidTokenLogInner>>,
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
            let handle = this.graph.insert(inner, debug_info);
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
    #[cfg(not(target_arch = "wasm32"))]
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

    #[cfg(target_arch = "wasm32")]
    fn current_rss_bytes() -> u64 {
        0
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
            dirty_marks_scratch: RefCell::new(Vec::with_capacity(256)),
            generation: Generation::new(),
            invalid_token_log: Rc::new(RefCell::new(InvalidTokenLogInner::default())),
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

    /// 输出 active_nodes 与 RSS，便于 GUI 与日志实时显示。
    #[must_use]
    pub fn metrics_snapshot(&self) -> AnchorsMetrics {
        let active = self.graph.with(|graph| graph.active_nodes());
        let rss = Self::current_rss_bytes();
        AnchorsMetrics {
            active_nodes: active,
            rss_bytes: rss,
        }
    }

    /// 读取 slotmap GC 计数（gc_skipped / free_skip / pending_free）。
    #[must_use]
    pub fn slotmap_gc_stats(&self) -> GcStatsSnapshot {
        self.graph.gc_stats_snapshot()
    }

    /// 读取 Anchor 存储池统计（alloc hit/miss/recycle + high watermark）。
    #[must_use]
    pub fn anchor_pool_stats(&self) -> AnchorPoolStatsSnapshot {
        self.graph.anchor_pool_stats_snapshot()
    }

    /// 进入 anchors epoch（读窗口）。
    ///
    /// 在 epoch 期间：
    /// - 禁止 token 的物理回收/复用（nodes.remove -> free list）；
    /// - 只允许把待回收节点 retire 入队，等到 epoch end 再统一 reclaim。
    ///
    /// 典型用法：由事件循环在 “事件派发 + stabilize + render” 外层持有该 guard。
    #[must_use]
    pub fn enter_epoch(&self) -> EpochGuard {
        EpochGuard::new(self.graph.clone())
    }

    /// 读取 token 计数与最近删除记录，便于验证“单调且不复用”。
    #[must_use]
    pub fn token_audit_snapshot(&self) -> TokenAudit {
        let next = self.graph.token_counter();
        let last_deleted = self.graph.last_deleted_token();
        TokenAudit {
            next_token: next,
            last_deleted_token: last_deleted,
        }
    }

    // ─────────────────────────────────────────────────────────────────────────
    // 失效 token 观测与去重

    fn invalid_token_backtrace_enabled() -> bool {
        static ENABLED: OnceLock<bool> = OnceLock::new();
        *ENABLED.get_or_init(|| emg_debug_env::bool_strict("ANCHORS_INVALID_TOKEN_BACKTRACE"))
    }

    fn bump_invalid_token_count(&self, raw_token: u64) -> u32 {
        let mut slot = self.invalid_token_log.borrow_mut();
        slot.bump(raw_token)
    }

    #[inline]
    fn should_log_invalid_token(count: u32) -> bool {
        matches!(count, 1 | 10 | 100) || count % 1000 == 0
    }

    // ─────────────────────────────────────────────────────────────────────────
    // “意外 Pending”观测与去重

    fn bump_unexpected_pending_count(&self, raw_token: u64) -> u32 {
        let mut slot = self.unexpected_pending_log.borrow_mut();
        slot.bump(raw_token)
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

    #[inline]
    fn fast_path_ready_value<O: Clone + 'static>(&self, anchor: &Anchor<O>) -> Option<O> {
        if !self.dirty_marks.borrow().is_empty() {
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
                    "slotmap: anchor locked or pending after stabilize {}",
                    node.debug_info_text()
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
        let inner_ptr = unsafe { *node.anchor.get() }.unwrap_or_else(|| {
            panic!(
                "slotmap: 节点已被删除或 token 失效，无法执行操作：{}，token={:?}",
                context,
                node.key()
            )
        });
        let inner = unsafe { inner_ptr.as_ref() };
        inner
            .output(&mut EngineContext { engine: self })
            .downcast_ref::<O>()
            .unwrap()
            .clone()
    }

    /// stabilize 8 轮未收敛时的残留子图 dump(方案 A M0 预铺, 为 M3 Kahn 死锁检测观察基线)。
    ///
    /// 现状(数值化调度)下不收敛意味着 recalc 队列仍有 Pending 节点反复 requeue;
    /// M2(去数值化)后不收敛的形态将变为"队列空 + Waiting 挂起非空", dump 内容届时演进。
    fn dump_unconverged_subgraph(&self) -> Vec<String> {
        self.graph
            .with(|graph| graph.collect_pending_nodes_brief(32))
    }

    ////////////////////////////////////////////////////////////////////////////////
    // 汇总当前是否仍有待处理的工作：
    // - dirty_marks: Var/VarVOA::set 注入的脏标记
    // - graph.recalc_pending: 已入队但尚未处理的重算
    // - pending_requests: slotmap+pending_queue 下的延迟请求
    //
    // 作用：
    // - stabilize 做“固定点收敛”时判断是否需要继续下一轮
    // - get/get_with/peek_value 判定当前输出是否可视为稳定
    ////////////////////////////////////////////////////////////////////////////////
    #[inline]
    fn has_pending_work(&self) -> bool {
        if !self.dirty_marks.borrow().is_empty() {
            return true;
        }
        let graph_pending = self
            .graph
            .with(|graph| graph.has_recalc_pending() || graph.has_pending_free());
        if graph_pending {
            return true;
        }
        false
    }

    pub fn get<O: Clone + 'static>(&mut self, anchor: &Anchor<O>) -> O {
        if let Some(val) = self.fast_path_ready_value(anchor) {
            return val;
        }

        ////////////////////////////////////////////////////////////////////////////////
        // 调度层架构调整后，stabilize 已负责 dirty/recalc/pending 的主循环收敛。
        //
        // 但仍保留一个“读后补轮”：
        // - 某些 output() 路径会在读取阶段触发 set（写回状态）；
        // - 这些 dirty 发生在 stabilize 之后，必须在同一次 get 内补吃掉，避免中间态外泄。
        ////////////////////////////////////////////////////////////////////////////////
        const MAX_POST_READ_ROUNDS: usize = 4;
        let mut rounds: usize = 0;
        loop {
            self.stabilize();
            let out = self.graph.with(|graph| {
                let anchor_node = self.ready_node(&graph, anchor.token(), "get::<O> 读取节点");
                self.read_ready_output(anchor_node, "get::<O> 读取节点")
            });

            let pending = self.has_pending_work();
            if !pending || rounds >= MAX_POST_READ_ROUNDS {
                if pending && rounds >= MAX_POST_READ_ROUNDS {
                    tracing::warn!(
                        target: "anchors",
                        rounds,
                        "get::<O> 读后补轮达到上限, 仍存在待处理工作"
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
        // 与 get 一致：保留“读后补轮”来吸收 output() 期间产生的新增 dirty。
        //
        // 约束：
        // - FnOnce 只能在最终稳定轮执行一次。
        ////////////////////////////////////////////////////////////////////////////////
        const MAX_POST_READ_ROUNDS: usize = 4;
        let mut rounds: usize = 0;
        let mut func = Some(func);
        loop {
            self.stabilize();
            let out = self.graph.with(|graph| {
                let target_node =
                    self.ready_node(&graph, anchor.token(), "get_with 读取 anchor/output");
                let inner_ptr =
                    unsafe { *target_node.anchor.get() }.expect("get_with: anchor 缺失");
                let o = unsafe { inner_ptr.as_ref() }
                    .output(&mut EngineContext { engine: self })
                    .downcast_ref::<O>()
                    .unwrap();
                o.clone()
            });

            let pending = self.has_pending_work();
            if !pending || rounds >= MAX_POST_READ_ROUNDS {
                if pending && rounds >= MAX_POST_READ_ROUNDS {
                    tracing::warn!(
                        target: "anchors",
                        rounds,
                        "get_with 读后补轮达到上限, 仍存在待处理工作"
                    );
                }
                let f = func.take().expect("get_with: func 已被消费");
                return f(&out);
            }

            rounds = rounds.saturating_add(1);
        }
    }

    /// 读取已 Ready 节点的输出副本，不触发重算。
    pub fn peek_value<O: Clone + 'static>(&self, anchor: &Anchor<O>) -> Option<O> {
        ////////////////////////////////////////////////////////////////////////////////
        // 关键语义：
        // - peek_value 只能在“引擎没有待处理工作”时返回缓存值；
        // - 否则（例如 Var::set 产生 dirty_marks，但尚未 stabilize）缓存值可能是“已过期”的旧值；
        // - 为了避免上层误读脏缓存，这里选择保守：只要引擎不 idle 就返回 None。
        ////////////////////////////////////////////////////////////////////////////////
        if self.has_pending_work() {
            return None;
        }
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
            ////////////////////////////////////////////////////////////////////////////////
            // 关键优化：dirty_marks 用 scratch 复用容量,避免 `mem::take` 造成的分配/回收风暴
            //
            // 语义要求：
            // - 我们必须在处理本轮 dirty_marks 时释放对 dirty_marks 的借用,
            //   以允许 stabilize 期间的 `Var::set` 继续 push 新 token；
            // - 同时,dirty_marks 自身最好保留容量,否则 push 会频繁触发 grow/alloc。
            //
            // 做法：
            // - 把当前 batch drain 到 `dirty_marks_scratch`；
            // - dirty_marks 变为空 Vec(但容量保留)；
            // - 处理 scratch,期间新 push 会进入 dirty_marks,留待下一轮/额外轮次收敛。
            ////////////////////////////////////////////////////////////////////////////////
            let mut scratch = self.dirty_marks_scratch.borrow_mut();
            scratch.clear();
            {
                let mut dirty_marks = self.dirty_marks.borrow_mut();
                let need = dirty_marks.len();
                let cap = scratch.capacity();
                if cap < need {
                    scratch.reserve(need - cap);
                }
                scratch.extend(dirty_marks.drain(..));
            }

            for dirty in scratch.drain(..) {
                if let Some(node) = graph.get(dirty) {
                    mark_dirty(graph, node, false);
                    // 确保自身也进入重算队列，避免特殊情况下 dirty 未触发队列（例如必要关系缺失）。
                    graph.queue_recalc(node);
                } else {
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
        self.generation.increment();
        self.update_dirty_marks();

        ////////////////////////////////////////////////////////////////////////////////
        // 调度层架构重构：在 stabilize 内部做“工作集固定点收敛”
        //
        // 背景：
        // - 过去 `stabilize` 主要处理 dirty_marks，`get/get_with` 再做外层重试；
        // - 这会把收敛职责拆散到两层，导致重复进入调度循环，放大 `request_node` 热点。
        //
        // 现在：
        // - 统一由 `stabilize` 负责 dirty/recalc/pending 的闭包收敛；
        // - `get/get_with` 只需调用一次 stabilize 并读取结果。
        //
        // 安全阀：
        // - 固定点轮次有上限，避免非收敛依赖导致事件循环长时间卡住。
        ////////////////////////////////////////////////////////////////////////////////
        const MAX_STABILIZE_ROUNDS: usize = 8;
        let mut rounds: usize = 0;
        let mut pending_work = true;
        while pending_work && rounds < MAX_STABILIZE_ROUNDS {
            rounds = rounds.saturating_add(1);
            #[cfg(debug_assertions)]
            trace!("stabilize: round={} pending_work={}", rounds, pending_work);
            self.stabilize_with_pending_queue();
            pending_work = self.has_pending_work();
            if pending_work && !self.dirty_marks.borrow().is_empty() {
                self.update_dirty_marks();
            }
        }

        // 诊断计数: stabilize 轮次直方图(未超限记实际轮数 1..8, 超限记 index 8)。
        waiting_record_rounds(rounds, pending_work);

        if pending_work {
            // 方案 A M0 预铺: 超限不收敛时附队列残留子图, 为 M3 Kahn 死锁检测积累基线形态。
            let residue = self.dump_unconverged_subgraph();
            tracing::warn!(
                target: "anchors",
                rounds = MAX_STABILIZE_ROUNDS,
                residue = ?residue,
                "stabilize: 调度工作集在上限轮次后仍未收敛（dirty/recalc/pending 仍存在），建议排查循环依赖或长期挂起链路"
            );
        }

        #[cfg(debug_assertions)]
        trace!("....stabilize");
    }

    /// internal function for stabilization. does not update dirty marks or increment the stabilization number
    fn stabilize0(&self) {
        self.stabilize_with_pending_queue();
    }

    /// 方案 A M2a: 读取点惰性收敛(lazy ensure)。
    ///
    /// 用于 EngineContext(Mut)::get 遇到非 Ready 节点的兜底: 动态内部 anchor
    /// (Either/snapshot view_gel 重建)经 request 分支一刚入队但排在 FIFO 后段,
    /// 数值化调度靠"新节点高度最低"排序兜底, FIFO 改为读取点就地跑一遍队列
    /// 固定点循环(队列耗尽即停; 正在重算的节点 anchor_locked 会被跳过, 无重入)。
    /// 队列耗尽后仍未 Ready(如脏源尚在 dirty_marks 未 flush)由调用方按原语义 panic。

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
                    // 启动阶段/依赖扩展时，合法的高度调整次数可能远超 active_nodes；
                    // 这里使用更保守的倍数，减少误报。
                    let active_nodes = graph.active_nodes();
                    std::cmp::max(5_000, active_nodes.saturating_mul(64))
                }
            } else {
                0
            };

            ////////////////////////////////////////////////////////////////////////////////
            // 常数优化: 默认关闭 spin_debug 时不分配 VecDeque。
            //
            // 背景:
            // - `stabilize_with_pending_queue` 是高频路径(每次 click/flush 都会进来)；
            // - 过去即使没开 `ANCHORS_DEBUG_SPIN`,也会 `with_capacity(32)` 触发一次堆分配；
            // - 在 list perf 场景下(数百次点击),这会放大 allocator/drop 噪声。
            ////////////////////////////////////////////////////////////////////////////////
            let mut spin_last: VecDeque<(u64, String)> = VecDeque::new();
            if spin_debug {
                spin_last = VecDeque::with_capacity(32);
            }
            graph.retry_pending_free();
            while let Some(node) = graph.recalc_pop_next() {
                // 诊断计数: 调度活动总量(成功弹出的重算节点数)。
                waiting_record_pop();
                if spin_debug {
                    spin_counter = spin_counter.saturating_add(1);
                    if spin_last.len() == 32 {
                        spin_last.pop_front();
                    }
                    spin_last.push_back((
                        node.key().raw_token(),
                        node.debug_info_text(),
                    ));

                    if spin_limit > 0 && spin_counter > spin_limit {
                        let mut msg = String::new();
                        let _ = writeln!(
                            &mut msg,
                            "stabilize0 spin detected: 重算次数超过上限 {spin_limit} (count={spin_counter})"
                        );
                        let active_nodes = graph.active_nodes();
                        let _ = writeln!(&mut msg, "active_nodes={active_nodes}");
                        let _ = writeln!(&mut msg, "recent_recalc (oldest -> newest):");
                        for (i, (tok, info)) in spin_last.iter().enumerate() {
                            let _ = writeln!(&mut msg, "  #{i} token={tok} node={info}");
                        }
                        panic!("{msg}");
                    }
                }
                // 方案 A M2a: FIFO 队列无"弹出高度 vs 当前高度"漂移问题(无传播),
                // 出队即重算。
                let calculation_complete = self.recalculate(graph, node);

                if !calculation_complete {
                    // 方案 A M2b: Pending 传染者摘除挂起(不再 requeue-to-tail,
                    // 消除提前 poll 白跑), 唤醒由依赖完成事件承载:
                    // 值 Updated -> mark_dirty 链; 值 Unchanged -> wake_waiting_dependents。
                    waiting_record_park();
                    graph.park_recalc(node);
                }
            }
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
        });
        #[cfg(debug_assertions)]
        trace!("...stabilize0");
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
                            node.debug_info_text(),
                            node.pending_recalc.get()
                        );
                    }
                    // 若锁期间收到“补偿重算”标记，解锁后再入队自身，既避免重入也不丢更新。
                    if node.pending_recalc.get() {
                        if debug_requeue_enabled() {
                            println!(
                                "REQUEUE pending node={:?} state_before={:?}",
                                node.debug_info_text(),
                                graph2::recalc_state(node),
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
                panic_anchor_locked_strict(node.key().raw_token(), node.debug_info_text(), trace);
            }
            let retry = node.recalc_retry.get().saturating_add(1);
            node.recalc_retry.set(retry);
            #[allow(clippy::manual_is_multiple_of)]
            if retry % 8 == 0 {
                tracing::warn!(
                    target: "anchors",
                    "检测到遗留 Anchor 锁，强制解锁后重算: {:?}, retry={}",
                    node.debug_info_text(),
                    retry
                );
            }
            node.anchor_locked.set(false);
        }
        node.anchor_locked.set(true);

        // 可选调试：设置 `ANCHORS_RECALC_TRACE=1` 可打印正在重算的节点信息，便于跟踪依赖链。
        if recalc_trace_enabled() {
            println!("RECALC {:?}", node.debug_info_text());
        }

        let trace_key = if lock_trace_enabled() {
            Some(node.key().raw_token())
        } else {
            None
        };
        if let Some(key) = trace_key {
            lock_trace_store(key, node.debug_info_text());
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
        let Some(mut anchor_ptr) = (unsafe { *this_anchor.get() }) else {
            if cfg!(debug_assertions) {
                // 若 anchor 为空但持有者计数大于 0，说明释放链路异常，应当尽快排查。
                debug_assert!(
                    node.handle_count() == 0,
                    "recalculate: anchor=None 但 handle_count>0，疑似非法释放路径，token={:?} debug={}",
                    node.key().raw_token(),
                    node.debug_info_text()
                );
            }
            return true;
        };

        // SAFETY:
        // - 该引用只在本次 recalculate 调用栈内使用；
        // - anchor_locked=true 约束了重入路径，避免同时存在多个可变引用。
        let anchor_ref: &mut dyn GenericAnchor = unsafe { anchor_ptr.as_mut() };
        {
            // 把之前因借用冲突积累的 dirty 消息在这里一次性补偿。
            // 刀B: 惰性 Box — None (绝大多数节点) 时零开销跳过。
            if let Some(pending) = node.pending_dirty.borrow_mut().as_deref_mut() {
                for child in pending.drain(..) {
                    anchor_ref.dirty(&child);
                }
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
                    node.debug_info_text(),
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
                let debug = node.debug_info_text();
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
                node.debug_info_text(),
            );
        };
        match poll_result {
            Poll::Pending => handle_waiting(Poll::Pending),
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
                    node.debug_info_text()
                );
                if node.last_ready.get().is_some() {
                    node.last_ready.set(Some(self.generation));
                    return true;
                }
                panic!(
                    "slotmap: 检测到失效 token request，且节点尚无历史输出无法降级；请开启 ANCHORS_INVALID_TOKEN_BACKTRACE=1 定位根因，node={}",
                    node.debug_info_text()
                );
            }
            Poll::Pending => handle_waiting(Poll::Pending),
            Poll::Updated => {
                if emg_debug_env::bool_lenient("ANCHORS_DEBUG_PARENTS") {
                    let parents = node.clean_parents();
                    println!(
                        "UPDATED {:?} parents={}",
                        node.debug_info_text(),
                        parents.len()
                    );
                }
                // make sure all parents are marked as dirty, and observed parents are recalculated
                mark_dirty(graph, node, true);
                node.last_update.set(Some(self.generation));
                node.last_ready.set(Some(self.generation));
                true
            }
            Poll::Updated => {
                if emg_debug_env::bool_lenient("ANCHORS_DEBUG_PARENTS") {
                    let parents = node.clean_parents();
                    println!(
                        "UPDATED {:?} parents={}",
                        node.debug_info_text(),
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
                // 方案 A M2b: 值未变也必须放行挂起在本节点上的 Waiting 依赖者
                // (它们等的是"完成"而非"值变"; 不发 dirty, 依赖者 re-poll 时用
                // read_gen 判定自然得出 Unchanged)。
                graph.wake_waiting_dependents(node);
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
        let mut visited: IndexSet<AnchorToken> = IndexSet::default();
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
        let mut labels: IndexMap<AnchorToken, String> = IndexMap::default();

        self.graph.with(|graph| {
            while let Some(tok) = queue.pop_front() {
                if let Some(node) = graph.get(tok) {
                    // 生成节点标签：优先使用 debug 位置信息
                    let dbg = node.debug_info_of();
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
        let audit = self.token_audit_snapshot();
        let last = audit
            .last_deleted_token
            .map(|v| v.to_string())
            .unwrap_or_else(|| "None".to_string());
        out.push_str(&format!(
            "  labelloc=\"t\";\n  label=\"token_next={} | last_deleted={}\";\n",
            audit.next_token, last
        ));

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
        let mut visited: IndexSet<AnchorToken> = IndexSet::default();
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
        let mut labels: IndexMap<AnchorToken, String> = IndexMap::default();

        self.graph.with(|graph| {
            while let Some(tok) = queue.pop_front() {
                if let Some(node) = graph.get(tok) {
                    let dbg = node.debug_info_of();
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
// ────────────────────────────────────────────────────────────────────────────
// 方案 A(去数值化调度)调度活动确定性计数(诊断设施, ANCHORS_WAITING_STATS=1 + 宿主退出打印)。
// 模式与 set_min_height 的 SMH_STATS 相同: 常开原子计数(Relaxed 足够便宜, 计数本身
// 已包含在历史基线口径内), 快照由宿主 env 门控打印(static 不保证 drop, 必须显式快照)。
//
// 计数语义(跨 M2 迁移对照设计, 详见 specs/anchors_plan_a_denumericalized_scheduling.md §5 M0):
// - pending_branch:       request_node 走保守分支返回 Pending 的次数
//                         (M2 前的"挂起"等价物; M2 后语义自然过渡为挂起次数);
// - parked:                stabilize 循环 Pending 传染后的挂起次数(M2b 起摘除, 不再 requeue);
// - wake_by_mark_dirty:   mark_dirty 唤醒链上的 queue_recalc 次数, 含 locked 补偿路径
//                         (外部输入变化引起的唤醒链活动量);
// - recalc_pops:          recalc_pop_next 成功弹出总数
//                         (调度活动总量; M2 前后对照"提前 poll 浪费"的核心指标);
// - stabilize_rounds[i]:  stabilize 实际使用轮次直方图(i=0..7 对应 1..8 轮, i=8 为超限)。
// ────────────────────────────────────────────────────────────────────────────
#[derive(Debug, Clone, Default)]
pub struct WaitingStatsSnapshot {
    pub pending_branch: u64,
    /// M2b 起 Pending 传染者挂起摘除(不再 requeue; M2a 时代此值为 requeue 次数)。
    pub parked: u64,
    pub wake_by_mark_dirty: u64,
    /// M2b: child 完成但值 Unchanged 时的挂起依赖者放行次数。
    pub wake_by_child_ready: u64,
    pub recalc_pops: u64,
    /// 9 槽直方图: index 0..7 = 收敛于 1..8 轮, index 8 = 8 轮超限。
    pub stabilize_rounds: [u64; 9],
}

struct WaitingStats {
    pending_branch: std::sync::atomic::AtomicU64,
    parked: std::sync::atomic::AtomicU64,
    wake_by_mark_dirty: std::sync::atomic::AtomicU64,
    wake_by_child_ready: std::sync::atomic::AtomicU64,
    recalc_pops: std::sync::atomic::AtomicU64,
    stabilize_rounds: [std::sync::atomic::AtomicU64; 9],
}
static WAITING_STATS: WaitingStats = WaitingStats {
    pending_branch: std::sync::atomic::AtomicU64::new(0),
    parked: std::sync::atomic::AtomicU64::new(0),
    wake_by_mark_dirty: std::sync::atomic::AtomicU64::new(0),
    wake_by_child_ready: std::sync::atomic::AtomicU64::new(0),
    recalc_pops: std::sync::atomic::AtomicU64::new(0),
    stabilize_rounds: [const { std::sync::atomic::AtomicU64::new(0) }; 9],
};

#[inline]
fn waiting_record_pending_branch() {
    WAITING_STATS
        .pending_branch
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}
#[inline]
fn waiting_record_park() {
    WAITING_STATS
        .parked
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}
/// M2b: wake_waiting_dependents(graph2)使用的跨模块计数入口。
#[inline]
pub(crate) fn waiting_record_wake_by_child_ready() {
    WAITING_STATS
        .wake_by_child_ready
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}
#[inline]
fn waiting_record_wake() {
    WAITING_STATS
        .wake_by_mark_dirty
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}
#[inline]
fn waiting_record_pop() {
    WAITING_STATS
        .recalc_pops
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}
#[inline]
fn waiting_record_rounds(rounds: usize, overflowed: bool) {
    let idx = if overflowed {
        8
    } else {
        rounds.saturating_sub(1).min(7)
    };
    WAITING_STATS.stabilize_rounds[idx].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// 读取调度活动诊断计数, 供宿主程序在退出路径显式打印(env 门控在宿主侧)。
/// S9 诊断: 按 anchor impl 类型统计活节点频次 top-20(需在 graph 上下文外调用,
/// 读取当前线程 DEFAULT_MOUNTER 的 engine 图)。
pub fn anchor_type_histogram() -> Vec<(String, usize)> {
    DEFAULT_MOUNTER.with(|m| {
        let borrowed = m.borrow();
        let Some(mounter) = borrowed.as_ref() else {
            return Vec::new();
        };
        mounter.graph.with(|g| g.anchor_type_histogram())
    })
}

pub fn waiting_stats_snapshot() -> WaitingStatsSnapshot {
    use std::sync::atomic::Ordering::Relaxed;
    let stabilize_rounds = WAITING_STATS
        .stabilize_rounds
        .each_ref()
        .map(|c| c.load(Relaxed));
    WaitingStatsSnapshot {
        pending_branch: WAITING_STATS.pending_branch.load(Relaxed),
        parked: WAITING_STATS.parked.load(Relaxed),
        wake_by_mark_dirty: WAITING_STATS.wake_by_mark_dirty.load(Relaxed),
        wake_by_child_ready: WAITING_STATS.wake_by_child_ready.load(Relaxed),
        recalc_pops: WAITING_STATS.recalc_pops.load(Relaxed),
        stabilize_rounds,
    }
}

fn mark_dirty<'a>(graph: Graph2Guard<'a>, node: NodeGuard<'a>, skip_self: bool) {
    #[cfg(debug_assertions)]
    trace!("mark_dirty {} ", node.debug_info_text());
    if skip_self {
        if debug_mark_enabled() {
            let parents_len = node.parents_len();
            println!(
                "mark_dirty skip_self node={:?} parents={}",
                node.debug_info_text(),
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
                        node.debug_info_text(),
                        parent.debug_info_text()
                    );
                }
                if lock_trace_enabled() {
                    tracing::warn!(
                        target: "anchors",
                        "skip queue_recalc: parent locked; child={:?} parent={:?}",
                        node.debug_info_text(),
                        parent.debug_info_text()
                    );
                }
                // 直接补偿入队一次：如果当下锁未释放，队列弹出时会跳过，等解锁后可正常重算。
                waiting_record_wake();
                graph.queue_recalc_force(parent);
                return;
            }

            // 常规路径：父节点未锁定时，直接下发 dirty，避免写入 pending_dirty 再补偿。
            if let Some(mut anchor_ptr) = unsafe { *parent.anchor.get() } {
                unsafe { anchor_ptr.as_mut().dirty(&child) };
            }

            waiting_record_wake();
            graph.queue_recalc(parent);
            mark_dirty0(graph, parent);
        });
    } else {
        mark_dirty0(graph, node);
    }
}

fn push_pending_dirty(parent: NodeGuard<'_>, child: NodeKey) {
    let mut pending = parent.pending_dirty.borrow_mut();
    // 刀B: 首次 deferred dirty 才分配 Box (低频补偿路径)
    let pending = pending.get_or_insert_with(|| Box::default());
    if !pending.contains(&child) {
        pending.push(child);
        if debug_dirty_flow_enabled() {
            println!(
                "PENDING_DIRTY child={:?} ({}) -> parent={:?} ({})",
                child.raw_token(),
                unsafe { child.ptr.lookup_unchecked() }.debug_info_text(),
                parent.key().raw_token(),
                parent.debug_info_text()
            );
        }
    }
}

#[inline(never)]
fn mark_dirty0<'a>(graph: Graph2Guard<'a>, next: NodeGuard<'a>) {
    #[cfg(debug_assertions)]
    trace!("mark_dirty0 {:?}", next);
    let id = next.key();
    ////////////////////////////////////////////////////////////////////////////////
    // 标脏时的依赖者传播(方案 A M2a 修正):
    //
    // - 数值化调度时代, observed 节点只入队自己、不递归依赖者 — 依赖者的重算
    //   顺序由高度序兜底(pop 序恒为 child -> parent), "依赖者读到中间态"的
    //   窗口不可达, 延迟到值真变时再通知(recalculate 完成点 mark_dirty)是纯优化;
    // - FIFO 调度(M2a 起)没有高度序兜底: B 入队期间, 依赖者 E 仍是 Ready 旧态,
    //   若 E 的依赖者 P 恰在此时 poll 并 request(E) 判 Updated, get 会读到
    //   "E Ready 但 E 的内部依赖 B Pending" 的中间态, 触发 get 契约 panic
    //   (实证: for_each_state insert_middle 首击, Either::output 内部 get)。
    // - 因此 observed 分支也必须同步向依赖者传播标脏(先 queue(child) 再
    //   queue(parents), FIFO 顺序内含因果序); 对数值化语义完全兼容(高度序不变,
    //   仅队列多出提前条目, 依赖者 Pending 白跑一次由 requeue 吸收)。
    ////////////////////////////////////////////////////////////////////////////////
    let propagate_to_parents = |next: &NodeGuard<'a>| {
        next.for_each_parent(|parent| {
            if parent.anchor_locked.get() {
                // 父节点正在重算：只记录 dirty，等待解锁后补偿入队。
                push_pending_dirty(parent, id);
                parent.pending_recalc.set(true);
                waiting_record_wake();
                graph.queue_recalc_force(parent);
                return;
            }

            // 常规路径：直接下发 dirty，避免写入 pending_dirty 再补偿。
            if let Some(mut anchor_ptr) = unsafe { *parent.anchor.get() } {
                unsafe { anchor_ptr.as_mut().dirty(&id) };
            }

            waiting_record_wake();
            graph.queue_recalc(parent);
            mark_dirty0(graph, parent);
        });
    };
    if Engine::check_observed_raw(next) != ObservedState::Unnecessary {
        // 如果节点当前正处于重算（anchor_locked=true），说明父链路已在调用栈上，无需再次入队以免触发重入检测。
        if !next.anchor_locked.get() {
            waiting_record_wake();
            graph.queue_recalc(next);
        }
        propagate_to_parents(&next);
    } else if graph2::recalc_state(next) == RecalcState::Ready {
        graph2::needs_recalc(next);
        if debug_mark_enabled() {
            println!(
                "mark_dirty0 unnecessary node={:?} parents={}",
                next.debug_info_text(),
                next.parents_len()
            );
        }
        propagate_to_parents(&next);
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

    fn owner_id(&self) -> Option<usize> {
        Some(Rc::as_ptr(&self.dirty_marks) as usize)
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
            ////////////////////////////////////////////////////////////////////////////////
            // 方案 A S2 修复: 读路径绝不触发重算(pull 读语义)。
            //
            // 原 M2b 的 lazy ensure(读取点嵌套 stabilize)是"自噬使能器": output 递归链
            // (Either::output -> get(either_anchor))中途的重算会重建子树, 旧 anchor 槽位
            // 被复用后, 继续执行的读取栈拿到新住客 -> downcast None 崩溃(实证: 第 6 次
            // 连续中插, probe Some 后正式调用 None, 中间仅隔嵌套重算)。
            //
            // 正确语义: token 校验通过 + 节点 handle 存活 => anchor impl 类型恒定,
            // 直接读其 output 缓存(可能略旧, 数值一致性由外层 stabilize 固定点保证);
            // 不校验 Ready(挂起中的节点同样可读旧值), 不做任何调度动作。
            ////////////////////////////////////////////////////////////////////////////////
            let anchor_ptr =
                unsafe { *node.anchor.get() }.expect("EngineContext::get: anchor 缺失");
            let anchor_impl = unsafe { anchor_ptr.as_ref() };
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
            // 方案 A S2 修复: 读路径绝不触发重算(pull 读语义, 语义与理由详见
            // EngineContext::get 的注释) — 挂起中的节点直接读 output 旧值。
            let anchor_ptr =
                unsafe { *node.anchor.get() }.expect("EngineContextMut::get: anchor 缺失");
            let anchor_impl = unsafe { anchor_ptr.as_ref() };
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
                    let slot_dbg_info = slot_debug.debug_info_text();

                    eprintln!(
                        "[anchors][invalid_token][request] parent_token={} parent={} child_token={} output_type={} necessary={} caller={}:{}:{} audit_next_token={} audit_last_deleted={:?} seen={} slot_token={} slot_anchor_none={} slot_anchor_locked={} slot_debug={}",
                        self.node.key().raw_token(),
                        self.node.debug_info_text(),
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
                        parent = %self.node.debug_info_text(),
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
                        parent = %self.node.debug_info_text(),
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
            //   - “依赖尚未 ready”（Pending）
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
                    parent = %self.node.debug_info_text(),
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

    fn dirty_handle_owner_id(&mut self) -> Option<usize> {
        Some(Rc::as_ptr(&self.engine.dirty_marks) as usize)
    }
}

impl<'eng, 'gg> EngineContextMut<'eng, 'gg> {
    fn request_node(&mut self, child: NodeGuard<'gg>, necessary: bool) -> Poll {
        ////////////////////////////////////////////////////////////////////////////////
        // 方案 A M2a: 建边不再做高度传播(ensure_height_increases 停用)。
        //
        // - 正确性由 M1 落地的边级 read_gen 判定 + mark_dirty 唤醒链 + stabilize
        //   固定点承载; FIFO 入队顺序内含因果序(mark_dirty0 先 queue(child) 再
        //   queue(parents); 分支一先 queue(C) 再挂 P)。
        // - 即时环检测(visited 随传播走)随之停用, 环将表现为 stabilize 不收敛
        //   (8 轮上限 warn + M0 的残留子图 dump), M3 补 Kahn 式死锁定位。
        ////////////////////////////////////////////////////////////////////////////////
        if debug_request_enabled() {
            println!(
                "REQUEST parent_token={} parent={:?} child_token={} child={:?} ready={:?}",
                self.node.key().raw_token(),
                self.node.debug_info_text(),
                child.key().raw_token(),
                child.debug_info_text(),
                graph2::recalc_state(child),
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
                    self.node.debug_info_text(),
                    child.key().raw_token(),
                    child.debug_info_text(),
                    graph2::recalc_state(child),
                );
            }
            if necessary && self_is_necessary {
                self.node.add_necessary_child(child);
            }
            self.defer_or_pending(child, necessary, self_is_necessary)
        } else {
            ////////////////////////////////////////////////////////////////////////////////
            // 方案 A M1: C Ready 时一律走边级 read_gen 判定。
            //
            // - 旧"分支二"(序刚被本次 request 抬高 -> Pending 等下一轮)删除:
            //   C Ready 意味着 C 不在队列、输出为当前稳定值, 直接读不破坏正确性,
            //   高度序仅是调度近似而非读取前提 — 这正是去数值化要建立的新世界观。
            // - 判定从节点级 (C.last_update vs P.last_ready) 换为边级
            //   (C.last_update vs link.read_gen), 语义见 graph2::ParentLink 注释;
            //   传播(ensure_height_increases)本阶段保留, SMH 计数应与基线一致。
            ////////////////////////////////////////////////////////////////////////////////
            let read_gen = child.add_clean_parent(self.node);
            if necessary && self_is_necessary {
                self.node.add_necessary_child(child);
            }
            match (child.last_update.get(), read_gen) {
                (Some(a), Some(v)) if a > v => {
                    // 消费了新版本: 写回边级记忆, 下次同值比较可判 Unchanged。
                    child.set_parent_link_read_gen(self.node.make_ptr(), Some(a));
                    Poll::Updated
                }
                // 同轮刚更新特判: 保留旧 fast path `a == b == generation => Updated` 语义
                // (C 在本轮 stabilize 内刚 Updated, poll 应看到新值)。
                (Some(a), Some(v)) if a == v && v == self.engine.generation => Poll::Updated,
                (Some(_), Some(_)) => Poll::Unchanged,
                // 边无版本记忆(P 从未完成过, read_gen=None)且 C 已有时间戳: 首读,
                // 必须写回版本 — 否则本边每次 request 都判 Updated, 会让 P 反复用
                // C 的透传型 output(如 cutoff/map)重算缓存(实证: test_cutoff_simple_observed
                // 135!=110 回归)。
                (Some(a), None) => {
                    child.set_parent_link_read_gen(self.node.make_ptr(), Some(a));
                    Poll::Updated
                }
                // C 自身无时间戳(从未 Updated 过): 无版本可记, 保守读。
                (None, _) => Poll::Updated,
            }
        }
    }

    fn defer_or_pending(
        &mut self,
        child: NodeGuard<'gg>,
        necessary: bool,
        self_is_necessary: bool,
    ) -> Poll {
        // 方案 A M4: pending_requests 重试表已随 anchors_pending_queue feature 移除,
        // 挂起唤醒由 M2b 的 Waiting 机制承载; 保守分支统一返回 Pending。
        let _ = (child, necessary, self_is_necessary);
        waiting_record_pending_branch();
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
