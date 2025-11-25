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
pub use graph2::NodeKey as AnchorToken;

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

use generation::Generation;
#[cfg(feature = "anchors_slotmap")]
use libc::RUSAGE_SELF;
#[cfg(feature = "anchors_slotmap")]
use libc::getrusage;
#[cfg(feature = "anchors_slotmap")]
use libc::rusage;
use std::any::Any;
use std::backtrace::Backtrace;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
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

    // tracks the current stabilization generation; incremented on every stabilize
    generation: Generation,
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
        }
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

    /// Retrieves the value of an Anchor, recalculating dependencies as necessary to get the
    /// latest value.
    pub fn get<O: Clone + 'static>(&mut self, anchor: &Anchor<O>) -> O {
        // 在读取前主动标记为 observed，确保父链路必要关系被建立，避免 clean_parents 被回收后无法向上脏传播。
        self.mark_observed(anchor);
        // stabilize once before, since the stabilization process may mark our requested node
        // as dirty
        self.stabilize();
        self.graph.with(|graph| {
            let anchor_node = self.expect_node(&graph, anchor.token(), "get::<O> 读取节点");
            if graph2::recalc_state(anchor_node) != RecalcState::Ready {
                trace!(
                    "graph2::recalc_state(anchor_node) != RecalcState::Ready ,{:?}",
                    anchor_node.debug_info.get()
                );
                graph.queue_recalc(anchor_node);
                // stabilize again, to make sure our target node that is now in the queue is up-to-date
                // use stabilize0 because no dirty marks have occured since last stabilization, and we want
                // to make sure we don't unnecessarily increment generation number
                self.stabilize0();
            }
            let mut retry = 0u8;
            loop {
                if anchor_node.anchor_locked.get() {
                    // 若节点残留锁且未在栈上重算，强制解锁并入队补偿，避免永远卡死。
                    anchor_node.anchor_locked.set(false);
                    graph.queue_recalc_force(anchor_node);
                    self.stabilize0();
                    retry = retry.saturating_add(1);
                    if retry > 3 {
                        panic!(
                            "slotmap: anchor locked after stabilize {:?}",
                            anchor_node.debug_info.get()
                        );
                    }
                    continue;
                }

                let target_anchor = unsafe { &*anchor_node.anchor.get() };
                if let Some(inner) = target_anchor.as_ref() {
                    break inner
                        .output(&mut EngineContext { engine: self })
                        .downcast_ref::<O>()
                        .unwrap()
                        .clone();
                } else {
                    panic!(
                        "slotmap: 节点已被删除或 token 失效，无法执行操作 get::<O> 读取节点，token={:?}",
                        anchor.token()
                    );
                }
            }
        })
    }
    pub fn get_with<O: Clone + 'static, F: FnOnce(&O) -> R, R>(
        &mut self,
        anchor: &Anchor<O>,
        func: F,
    ) -> R {
        self.mark_observed(anchor);
        // stabilize once before, since the stabilization process may mark our requested node
        // as dirty
        self.stabilize();
        self.graph.with(|graph| {
            let anchor_node = self.expect_node(&graph, anchor.token(), "get_with 读取节点");
            if graph2::recalc_state(anchor_node) != RecalcState::Ready {
                graph.queue_recalc(anchor_node);
                // stabilize again, to make sure our target node that is now in the queue is up-to-date
                // use stabilize0 because no dirty marks have occured since last stabilization, and we want
                // to make sure we don't unnecessarily increment generation number
                self.stabilize0();
            }
            let target_node = self.expect_node(&graph, anchor.token(), "get_with 读取 anchor");
            if target_node.anchor_locked.get() {
                graph.queue_recalc(target_node);
                self.stabilize0();
            }
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
        self.stabilize0();
        trace!("....stabilize");
    }

    /// internal function for stabilization. does not update dirty marks or increment the stabilization number
    fn stabilize0(&self) {
        trace!("stabilize0");
        self.graph.with(|graph| {
            // 可选调试：检测重算队列是否异常“打转”。设置 `ANCHORS_DEBUG_SPIN=1` 时启用。
            let mut spin_counter: usize = 0;
            let spin_debug = std::env::var("ANCHORS_DEBUG_SPIN")
                .map(|v| v != "0")
                .unwrap_or(false);
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
                    if spin_counter > 50_000 {
                        panic!(
                            "stabilize0 spin detected: 超过 50_000 次重算仍未收敛，最后节点={}",
                            node.debug_info.get()._to_string()
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
                    tracing::info!(
                        target: "anchors",
                        active_nodes = active,
                        rss_bytes = rss,
                        "anchors.active_nodes snapshot"
                    );
                }
            }
        });
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
        };
        /// ══════════════════════════════════════════════════════════════════════
        /// 锁严格模式开关：设置环境变量 `ANCHORS_LOCK_STRICT=1` 时，检测到遗留锁会直接 panic，
        /// 便于在本地/CI 及时暴露潜在的重入缺陷；默认模式则会抢占解锁继续重算，避免卡死。
        fn lock_strict_enabled() -> bool {
            static STRICT: OnceLock<bool> = OnceLock::new();
            *STRICT.get_or_init(|| {
                std::env::var("ANCHORS_LOCK_STRICT")
                    .map(|v| v != "0")
                    .unwrap_or(false)
            })
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
                .get_or_init(|| Mutex::new(HashMap::new()))
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
                .get_or_init(|| Mutex::new(HashMap::new()))
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
                .get_or_init(|| Mutex::new(HashMap::new()))
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
            flag: &'a Cell<bool>,
            trace_key: Option<u64>,
            node: NodeGuard<'a>,
            graph: Graph2Guard<'a>,
        }
        impl Drop for AnchorLockGuard<'_> {
            fn drop(&mut self) {
                self.flag.set(false);
                if let Some(key) = self.trace_key.take() {
                    let _ = lock_trace_take(key);
                }
                if std::env::var("ANCHORS_DEBUG_REQUEUE")
                    .map(|v| v != "0")
                    .unwrap_or(false)
                {
                    println!(
                        "REQUEUE check node={:?} pending_recalc={}",
                        self.node.debug_info.get()._to_string(),
                        self.node.pending_recalc.get()
                    );
                }
                // 若锁期间收到“补偿重算”标记，解锁后再入队自身，既避免重入也不丢更新。
                if self.node.pending_recalc.get() {
                    if std::env::var("ANCHORS_DEBUG_REQUEUE")
                        .map(|v| v != "0")
                        .unwrap_or(false)
                    {
                        println!(
                            "REQUEUE pending node={:?} state_before={:?} height={}",
                            self.node.debug_info.get()._to_string(),
                            graph2::recalc_state(self.node),
                            graph2::height(self.node)
                        );
                    }
                    self.node.pending_recalc.set(false);
                    // 强制入队：跳过 locked/Pending 判定。
                    self.graph.queue_recalc_force(self.node);
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
            flag: &node.anchor_locked,
            trace_key,
            node,
            graph,
        };

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
            Poll::Pending => {
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
        use std::collections::{HashMap, HashSet, VecDeque};

        // 使用 BFS 遍历从 roots 可达的子图
        let mut visited: HashSet<AnchorToken> = HashSet::new();
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
        let mut labels: HashMap<AnchorToken, String> = HashMap::new();

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
        names: &std::collections::HashMap<AnchorToken, String>,
    ) -> String {
        use std::collections::{HashMap, HashSet, VecDeque};

        let mut visited: HashSet<AnchorToken> = HashSet::new();
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
        let mut labels: HashMap<AnchorToken, String> = HashMap::new();

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
            let parents_len = node.clean_parents().len();
            println!(
                "mark_dirty skip_self node={:?} parents={}",
                node.debug_info.get()._to_string(),
                parents_len
            );
        }
        let parents = node.drain_clean_parents();
        for parent in parents {
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
                continue;
            }
            graph.queue_recalc(parent);
            mark_dirty0(graph, parent);
        }
    } else {
        mark_dirty0(graph, node);
    }
}

fn push_pending_dirty(parent: NodeGuard<'_>, child: NodeKey) {
    let mut pending = parent.pending_dirty.borrow_mut();
    if !pending.iter().any(|k| *k == child) {
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
        let parents = next.clean_parents();
        if std::env::var("ANCHORS_DEBUG_MARK")
            .map(|v| v != "0")
            .unwrap_or(false)
        {
            println!(
                "mark_dirty0 unnecessary node={:?} parents={}",
                next.debug_info.get()._to_string(),
                parents.len()
            );
        }
        for parent in parents {
            push_pending_dirty(parent, id);
            if !parent.anchor_locked.get() {
                graph.queue_recalc(parent);
                mark_dirty0(graph, parent);
            }
        }
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
        let height_already_increased = match graph2::ensure_height_increases(child, self.node) {
            Ok(v) => v,
            Err(()) => {
                panic!("loop detected in anchors!\n");
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
            // 即便子节点尚未计算完成，也先登记父链，避免后续 dirty 无父可递归导致更新丢失。
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
            Poll::Pending
        } else if !height_already_increased {
            // 高度刚被抬升，还没正式记录依赖；先挂上父链，确保第一次重算完成后 dirty 能向上传播。
            child.add_clean_parent(self.node);
            self.pending_on_anchor_get = true;
            Poll::Pending
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
    fn _to_string(&self) -> String {
        match self.location {
            Some((name, location)) => format!("{} ({})", location, name),
            None => self.type_info.to_string(),
        }
    }
}
