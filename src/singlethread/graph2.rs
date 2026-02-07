use super::anchor_pool::{AnchorMemPool, AnchorPoolStatsSnapshot};
use super::{AnchorDebugInfo, Generation, GenericAnchor};
use super::{Engine, EngineContext};
use std::backtrace::Backtrace;
use std::cell::{Cell, RefCell, UnsafeCell};
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::OnceLock;

// ─────────────────────────────────────────────────────────────────────────────
// Debug 开关缓存（热路径）
//
// 背景：
// - `add_clean_parent/queue_recalc/recalc_pop_next` 会在稳定化过程中被频繁调用；
// - 若在这些路径里反复查询 env（字符串查找/比较），会放大到明显的常数开销。
//
// 策略：
// - 仅在 debug_assertions 下允许通过 env 打印调试日志；
// - release/bench 下直接返回 false，保证热路径零额外开销。
// ─────────────────────────────────────────────────────────────────────────────
#[inline]
fn debug_parent_link_enabled() -> bool {
    #[cfg(debug_assertions)]
    {
        static ENABLED: OnceLock<bool> = OnceLock::new();
        *ENABLED.get_or_init(|| emg_debug_env::bool_lenient("ANCHORS_DEBUG_PARENT_LINK"))
    }

    #[cfg(not(debug_assertions))]
    {
        false
    }
}

#[inline]
fn debug_parent_flow_enabled() -> bool {
    #[cfg(debug_assertions)]
    {
        static ENABLED: OnceLock<bool> = OnceLock::new();
        *ENABLED.get_or_init(|| emg_debug_env::bool_lenient("ANCHORS_DEBUG_PARENT_FLOW"))
    }

    #[cfg(not(debug_assertions))]
    {
        false
    }
}

#[inline]
fn debug_queue_enabled() -> bool {
    #[cfg(debug_assertions)]
    {
        static ENABLED: OnceLock<bool> = OnceLock::new();
        *ENABLED.get_or_init(|| emg_debug_env::bool_lenient("ANCHORS_DEBUG_QUEUE"))
    }

    #[cfg(not(debug_assertions))]
    {
        false
    }
}

#[derive(Debug)]
struct FreeTraceConfig {
    enabled: bool,
    token_filter: Option<u64>,
    match_substr: Option<&'static str>,
    backtrace: bool,
}

#[derive(Debug)]
struct TokenTraceConfig {
    enabled: bool,
    token_filter: Option<u64>,
    match_substr: Option<&'static str>,
    backtrace: bool,
}

impl FreeTraceConfig {
    fn from_env() -> Self {
        ////////////////////////////////////////////////////////////////////////////////
        // NOTE:
        // - 这是纯调试能力：用于定位“某个 token 在谁/何时被 free”。
        // - 默认关闭，避免在正常运行时产生额外分支与日志。
        //
        // 用法（示例）：
        // - ANCHORS_TRACE_FREE=1 ANCHORS_TRACE_FREE_TOKEN=4799 ANCHORS_TRACE_FREE_BACKTRACE=1
        // - ANCHORS_TRACE_FREE=1 ANCHORS_TRACE_FREE_MATCH=node_builder.rs:169
        ////////////////////////////////////////////////////////////////////////////////
        let enabled = emg_debug_env::bool_strict("ANCHORS_TRACE_FREE");
        let token_filter = emg_debug_env::u64_opt("ANCHORS_TRACE_FREE_TOKEN");
        let match_substr = emg_debug_env::str_non_empty("ANCHORS_TRACE_FREE_MATCH");
        let backtrace = emg_debug_env::bool_strict("ANCHORS_TRACE_FREE_BACKTRACE");

        Self {
            enabled,
            token_filter,
            match_substr,
            backtrace,
        }
    }

    #[inline]
    fn should_log(&self, token: u64, debug_info: &str) -> bool {
        if !self.enabled {
            return false;
        }
        if let Some(filter) = self.token_filter {
            if token != filter {
                return false;
            }
        }
        if let Some(substr) = self.match_substr {
            if !debug_info.contains(substr) {
                return false;
            }
        }
        true
    }
}

fn free_trace_cfg() -> &'static FreeTraceConfig {
    static CFG: OnceLock<FreeTraceConfig> = OnceLock::new();
    CFG.get_or_init(FreeTraceConfig::from_env)
}

impl TokenTraceConfig {
    fn from_env(prefix: &str) -> Self {
        let enabled = emg_debug_env::bool_strict(prefix);

        let token_key = format!("{prefix}_TOKEN");
        let token_filter = emg_debug_env::u64_opt(token_key.as_str());

        let match_key = format!("{prefix}_MATCH");
        let match_substr = emg_debug_env::str_non_empty(match_key.as_str());

        let backtrace_key = format!("{prefix}_BACKTRACE");
        let backtrace = emg_debug_env::bool_strict(backtrace_key.as_str());

        Self {
            enabled,
            token_filter,
            match_substr,
            backtrace,
        }
    }

    #[inline]
    fn should_log(&self, token: u64, debug_info: &str) -> bool {
        if !self.enabled {
            return false;
        }
        if let Some(filter) = self.token_filter {
            if token != filter {
                return false;
            }
        }
        if let Some(substr) = self.match_substr {
            if !debug_info.contains(substr) {
                return false;
            }
        }
        true
    }
}

fn remove_trace_cfg() -> &'static TokenTraceConfig {
    static CFG: OnceLock<TokenTraceConfig> = OnceLock::new();
    CFG.get_or_init(|| TokenTraceConfig::from_env("ANCHORS_TRACE_REMOVE"))
}

fn reuse_trace_cfg() -> &'static TokenTraceConfig {
    static CFG: OnceLock<TokenTraceConfig> = OnceLock::new();
    CFG.get_or_init(|| TokenTraceConfig::from_env("ANCHORS_TRACE_REUSE"))
}

mod node_store {
    use slotmap::{DefaultKey, SlotMap};
    use std::cell::UnsafeCell;
    use std::hash::Hasher;
    use std::marker::PhantomData;

    pub struct NodeStore<T> {
        slots: UnsafeCell<SlotMap<DefaultKey, T>>,
    }

    impl<T> NodeStore<T> {
        pub fn new() -> Self {
            Self {
                slots: UnsafeCell::new(SlotMap::with_key()),
            }
        }

        pub fn with<F, R>(&self, func: F) -> R
        where
            F: for<'any> FnOnce(NodeStoreGuard<'any, T>) -> R,
        {
            let guard = NodeStoreGuard {
                graph: self,
                _marker: PhantomData,
            };
            func(guard)
        }

        pub unsafe fn with_unchecked(&self) -> NodeStoreGuard<'_, T> {
            NodeStoreGuard {
                graph: self,
                _marker: PhantomData,
            }
        }
    }

    #[derive(Debug)]
    pub struct NodeStoreGuard<'a, T> {
        pub(super) graph: *const NodeStore<T>,
        _marker: PhantomData<&'a NodeStore<T>>,
    }

    impl<'a, T> NodeStoreGuard<'a, T> {
        pub fn insert(&self, node: T) -> NodeGuard<'a, T> {
            unsafe {
                let slots = &mut *(*self.graph).slots.get();
                let key = slots.insert(node);
                NodeGuard {
                    graph: self.graph,
                    key,
                    _marker: PhantomData,
                }
            }
        }

        pub unsafe fn lookup_ptr(&self, ptr: NodePtr<T>) -> &T {
            let slots = unsafe { &*(*self.graph).slots.get() };
            ////////////////////////////////////////////////////////////////////////////////
            // 性能关键点（anchors_slotmap）：
            //
            // - 我们并不会对 SlotMap 执行物理 remove（仅把 Node 的 anchor 置空并放入 free list 复用）。
            // - 因此，SlotMap 内的 key 永远在 bounds 内，且不会出现版本失配导致的 None。
            //
            // 结论：
            // - debug 下保留 `.get(...).expect(...)` 以捕获越界/跨 Graph2 的 misuse；
            // - release/bench 下改用 `get_unchecked` 跳过 version/bounds 检查，减少热路径常数开销。
            ////////////////////////////////////////////////////////////////////////////////
            #[cfg(debug_assertions)]
            {
                debug_assert_eq!(ptr.graph, self.graph);
                return slots
                    .get(ptr.key)
                    .expect("dangling NodePtr: 已释放或跨 Graph2 使用");
            }

            #[cfg(not(debug_assertions))]
            {
                if ptr.graph != self.graph {
                    panic!("dangling NodePtr: 跨 Graph2 使用");
                }
                // SAFETY:
                // - ptr.key 来自 SlotMap::insert，且我们不做物理 remove，key 永远有效；
                // - 上面额外校验 ptr.graph，避免跨 Graph2 误用导致的越界 UB。
                return unsafe { slots.get_unchecked(ptr.key) };
            }
        }
    }

    impl<'a, T> Copy for NodeStoreGuard<'a, T> {}
    impl<'a, T> Clone for NodeStoreGuard<'a, T> {
        fn clone(&self) -> Self {
            *self
        }
    }

    pub struct NodePtr<T> {
        pub(super) graph: *const NodeStore<T>,
        pub(super) key: DefaultKey,
    }

    impl<T> NodePtr<T> {
        pub fn new(graph: *const NodeStore<T>, key: DefaultKey) -> Self {
            Self { graph, key }
        }

        pub unsafe fn lookup_unchecked<'a>(self) -> NodeGuard<'a, T> {
            NodeGuard {
                graph: self.graph,
                key: self.key,
                _marker: PhantomData,
            }
        }
    }

    impl<T> Copy for NodePtr<T> {}
    impl<T> Clone for NodePtr<T> {
        fn clone(&self) -> Self {
            *self
        }
    }

    impl<T> std::fmt::Debug for NodePtr<T> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("NodePtr")
                .field("graph", &(self.graph as *const ()))
                .field("key", &self.key)
                .finish()
        }
    }

    impl<T> PartialEq for NodePtr<T> {
        fn eq(&self, other: &Self) -> bool {
            self.graph == other.graph && self.key == other.key
        }
    }
    impl<T> Eq for NodePtr<T> {}

    impl<T> std::hash::Hash for NodePtr<T> {
        fn hash<H: Hasher>(&self, state: &mut H) {
            std::hash::Hash::hash(&(self.graph as usize), state);
            std::hash::Hash::hash(&self.key, state);
        }
    }

    impl<T> PartialOrd for NodePtr<T> {
        fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
            Some(self.cmp(other))
        }
    }

    impl<T> Ord for NodePtr<T> {
        fn cmp(&self, other: &Self) -> std::cmp::Ordering {
            self.key
                .cmp(&other.key)
                .then_with(|| (self.graph as usize).cmp(&(other.graph as usize)))
        }
    }

    pub struct NodeGuard<'a, T> {
        pub(super) graph: *const NodeStore<T>,
        pub(super) key: DefaultKey,
        _marker: PhantomData<&'a T>,
    }

    impl<'a, T> NodeGuard<'a, T> {
        pub fn make_ptr(&self) -> NodePtr<T> {
            NodePtr::new(self.graph, self.key)
        }

        pub fn node(&self) -> &T {
            let slots = unsafe { &*(*self.graph).slots.get() };
            ////////////////////////////////////////////////////////////////////////////////
            // 性能关键点（anchors_slotmap）：
            //
            // - `NodeGuard` 在热路径中被频繁 `Deref`；
            // - 若每次都走 `SlotMap::get`，会重复做 version/bounds 检查，在线性链路上放大为可见成本。
            //
            // 这里沿用与 `lookup_ptr` 相同的安全前提：
            // - debug 下保留检查，release/bench 下用 get_unchecked。
            ////////////////////////////////////////////////////////////////////////////////
            #[cfg(debug_assertions)]
            {
                return slots
                    .get(self.key)
                    .expect("dangling NodeGuard: 节点已被移除");
            }

            #[cfg(not(debug_assertions))]
            {
                // SAFETY:
                // - self.key 来自 SlotMap::insert 且不会被物理 remove，因此恒有效。
                return unsafe { slots.get_unchecked(self.key) };
            }
        }

        pub unsafe fn lookup_ptr(&self, ptr: NodePtr<T>) -> &T {
            let slots = unsafe { &*(*self.graph).slots.get() };
            #[cfg(debug_assertions)]
            {
                debug_assert_eq!(ptr.graph, self.graph);
                return slots.get(ptr.key).expect("dangling NodePtr: 节点已被移除");
            }

            #[cfg(not(debug_assertions))]
            {
                if ptr.graph != self.graph {
                    panic!("dangling NodePtr: 跨 Graph2 使用");
                }
                // SAFETY:
                // - ptr.key 来自 SlotMap::insert 且不会被物理 remove，因此恒有效；
                // - 校验 ptr.graph，避免跨 Graph2 误用导致的越界 UB。
                return unsafe { slots.get_unchecked(ptr.key) };
            }
        }
    }

    impl<'a, T> std::ops::Deref for NodeGuard<'a, T> {
        type Target = T;
        fn deref(&self) -> &Self::Target {
            self.node()
        }
    }

    impl<'a, T> Copy for NodeGuard<'a, T> {}
    impl<'a, T> Clone for NodeGuard<'a, T> {
        fn clone(&self) -> Self {
            *self
        }
    }

    impl<'a, T> PartialEq for NodeGuard<'a, T> {
        fn eq(&self, other: &Self) -> bool {
            self.graph == other.graph && self.key == other.key
        }
    }
    impl<'a, T> Eq for NodeGuard<'a, T> {}

    impl<'a, T> std::fmt::Debug for NodeGuard<'a, T> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("NodeGuard")
                .field("graph", &(self.graph as *const ()))
                .field("key", &self.key)
                .finish()
        }
    }

    impl NodeStore<super::Node> {
        /// 移除指定节点：解绑队列/父子指针并回收到 free list。
        ///
        /// # Safety
        /// 仅在确保 handle_count 已降为 0 且 token 匹配的场景调用。
        pub unsafe fn remove(&self, ptr: NodePtr<super::Node>) -> bool {
            let raw_guard = unsafe { ptr.lookup_unchecked() };
            let guard = super::NodeGuard(raw_guard);
            // unsafe: graph 指针由 slotmap 生成，与当前节点同生灭。
            let graph: &super::Graph2 = unsafe { &*guard.ptrs.graph };

            ////////////////////////////////////////////////////////////////////////////////
            // NOTE:
            // - 纯调试能力：定位“某个 token 在何处被 remove()（进入 free list）”。
            // - 该日志发生在真正执行 remove 的位置，便于区分 free() 与 retry_pending_free。
            //
            // 用法（示例）：
            // - ANCHORS_TRACE_REMOVE=1 ANCHORS_TRACE_REMOVE_TOKEN=1012 ANCHORS_TRACE_REMOVE_BACKTRACE=1
            // - ANCHORS_TRACE_REMOVE=1 ANCHORS_TRACE_REMOVE_MATCH=node_builder.rs
            ////////////////////////////////////////////////////////////////////////////////
            {
                let cfg = super::remove_trace_cfg();
                if cfg.enabled {
                    let token = guard.slot_token.get();
                    let debug_info = guard.debug_info.get()._to_string();
                    if cfg.should_log(token, debug_info.as_str()) {
                        ////////////////////////////////////////////////////////////////////////////////
                        // NOTE:
                        // - remove() 发生时，理论上 handle_count 应该已经归零（否则会导致“活句柄变 stale”）。
                        // - 这里把关键状态一起打印出来，便于定位 invalid_token 的根因。
                        ////////////////////////////////////////////////////////////////////////////////
                        let handle_count = guard.ptrs.handle_count.get();
                        let necessary_count = guard.necessary_count.get();
                        let anchor_locked = guard.anchor_locked.get();
                        let anchor_is_none = unsafe { (&*guard.anchor.get()).is_none() };
                        eprintln!(
                            "[anchors][remove] token={token} handle_count={handle_count} necessary_count={necessary_count} anchor_locked={anchor_locked} anchor_is_none={anchor_is_none} debug={debug_info}"
                        );
                        if cfg.backtrace {
                            let bt = super::Backtrace::force_capture();
                            eprintln!("[anchors][remove] backtrace:\n{bt}");
                        }
                    }
                }
            }

            graph.inc_free_attempt();
            if guard.anchor_locked.get() {
                graph.inc_gc_skipped();
                return false;
            }

            if unsafe { (&*guard.anchor.get()).is_none() } {
                graph.inc_free_skip();
                return true;
            }

            ////////////////////////////////////////////////////////////////////////////////
            // 关键修复：free/remove 时必须解绑 “child -> parent” 的 clean_parents 指针。
            //
            // 背景：
            // - 依赖边是双向维护的：
            //   - parent.ptrs.necessary_children 记录输入 child
            //   - child.ptrs.clean_parents 记录依赖者 parent
            // - 过去我们只清了 parent.necessary_children，但没有把 parent 从 child.clean_parents 移除。
            // - 在 slotmap/free list “复用槽位”后，旧的 NodePtr 会指向新的节点内容，
            //   导致 child 更新时把 dirty 误投递给新节点（常见表现：Constant/Var 的 dirty 报错刷屏）。
            //
            // 做法：
            // - 在真正进入 free list 之前，先把本节点从所有 children 的 clean_parents 中移除；
            // - 全程使用 try_borrow_mut：一旦遇到外部借用冲突，放弃本次 remove，等待下次重试。
            ////////////////////////////////////////////////////////////////////////////////

            ////////////////////////////////////////////////////////////////////////////////
            // 关键改良：先解绑边并释放 RefMut，再 drop anchor
            //
            // 背景：
            // - drop `Box<dyn GenericAnchor>` 会级联 drop 大量 `AnchorHandle`，并触发重入 free/remove；
            // - 如果此时仍持有 parent.ptrs.clean_parents / parent.ptrs.necessary_children 的 RefMut，
            //   重入的 remove 可能因为借用冲突失败，导致 gc_skipped + retry，形成额外常数开销。
            //
            // 做法：
            // - 将“解绑 + clear”放入独立作用域，作用域结束后 RefMut 自动释放；
            // - 再进行 dequeue 与 drop anchor，最大化降低重入借用冲突概率。
            ////////////////////////////////////////////////////////////////////////////////
            {
                // 预检：父列表（依赖者列表）同样用 try_borrow_mut，避免外层仍持有借用导致 panic。
                let Ok(mut self_parents) = guard.ptrs.clean_parents.try_borrow_mut() else {
                    graph.inc_gc_skipped();
                    return false;
                };

                // 尝试非阻塞清理必要子节点，若外部仍借用则跳过以避免 RefCell panic。
                let Ok(mut children) = guard.ptrs.necessary_children.try_borrow_mut() else {
                    graph.inc_gc_skipped();
                    return false;
                };

                ////////////////////////////////////////////////////////////////////////////////
                // 性能改良：避免为 children_snapshot 分配 Vec
                //
                // 说明：
                // - 原实现会 `collect()` 一份 children_snapshot 用于两次遍历（预检/解绑）；
                // - remove() 在 UI 高频增删时会被大量调用，这个额外分配会成为稳定常数开销；
                // - 这里直接对 `children` 做两次迭代，避免额外分配。
                ////////////////////////////////////////////////////////////////////////////////

                // 预检：确保所有 child 的 clean_parents 都可写，避免“解绑做到一半就失败”的不一致状态。
                for child_ptr in children.iter().copied() {
                    let child = unsafe { guard.0.lookup_ptr(child_ptr) };
                    if child.ptrs.clean_parents.try_borrow_mut().is_err() {
                        graph.inc_gc_skipped();
                        return false;
                    }
                }

                // 执行解绑：从每个 child.clean_parents 移除当前 parent 指针，并维护 necessary_count。
                for child_ptr in children.iter().copied() {
                    let child = unsafe { guard.0.lookup_ptr(child_ptr) };

                    // fast path：若 parent 恰好在 clean_parent0，直接清空。
                    if child
                        .ptrs
                        .clean_parent0
                        .get()
                        .is_some_and(|link| link.key.ptr == ptr)
                    {
                        child.ptrs.clean_parent0.set(None);
                    }
                    // fast path：若 parent 恰好在 clean_parent1，直接清空。
                    if child
                        .ptrs
                        .clean_parent1
                        .get()
                        .is_some_and(|link| link.key.ptr == ptr)
                    {
                        child.ptrs.clean_parent1.set(None);
                    }

                    // slow path：在 clean_parents Vec 中移除所有等于 ptr 的条目（允许重复）。
                    let Ok(mut child_parents) = child.ptrs.clean_parents.try_borrow_mut() else {
                        // 兜底：预检已通过，这里不应失败；若失败，视为并发借用冲突，延后回收。
                        graph.inc_gc_skipped();
                        return false;
                    };
                    if !child_parents.is_empty() {
                        child_parents.retain(|link| link.key.ptr != ptr);
                    }

                    child
                        .necessary_count
                        .set(child.necessary_count.get().saturating_sub(1));
                }

                // 清空 parent 的必要子节点列表（输入边）。
                children.clear();

                // 清空 parent 的父列表（被依赖者列表）。
                guard.ptrs.clean_parent0.set(None);
                guard.ptrs.clean_parent1.set(None);
                self_parents.clear();
            }

            ////////////////////////////////////////////////////////////////////////////////
            // 关键：节点即将进入 free list，必须确保它已经彻底从 recalc 队列摘除。
            //
            // 否则：
            // - recalc_pop_next 可能在之后把 free list 的 next 当成 queue next；
            // - 进而污染 free list，导致活节点被误复用，产生 stale token 与 invalid_token request。
            //
            // 这里不信任 `recalc_state/height`：
            // - 优先用 NodePtr + recalc_bucket O(1) 摘除；
            // - 必要时兜底扫描所有 bucket（极少数路径）。
            ////////////////////////////////////////////////////////////////////////////////
            super::dequeue_calc_by_ptr(graph, ptr);

            let deleted_token = guard.slot_token.get();
            ////////////////////////////////////////////////////////////////////////////////
            // 关键改良：先把 anchor_slot 置 None，再 drop 旧 anchor
            //
            // 背景：
            // - drop 旧 anchor 会触发一串重入 free/remove；
            // - 我们希望重入路径更容易把本节点识别为 dead（anchor=None），避免额外链路工作与借用冲突。
            ////////////////////////////////////////////////////////////////////////////////
            let old_anchor = unsafe { (&mut *guard.anchor.get()).take() };
            if let Some(old_anchor) = old_anchor {
                // 先析构，再回收 raw block（pool），减少 alloc/free 常数开销。
                graph.drop_and_recycle_anchor_ptr(old_anchor);
            } else {
                return true;
            }

            guard.ptrs.handle_count.set(0);
            guard.observed.set(false);
            guard.visited.set(false);
            guard.necessary_count.set(0);
            guard.ptrs.recalc_state.set(super::RecalcState::Needed);
            guard.ptrs.recalc_bucket.set(None);
            guard.recalc_retry.set(0);
            guard.pending_dirty.borrow_mut().clear();
            guard.pending_recalc.set(false);
            guard.anchor_locked.set(false);

            let free_head = &graph.free_head;
            if let Some(old_free) = free_head.get() {
                let old_guard = unsafe { old_free.lookup_unchecked() };
                old_guard.ptrs.free_prev.set(Some(ptr));
            }
            guard.ptrs.free_next.set(free_head.get());
            guard.ptrs.free_prev.set(None);
            free_head.set(Some(ptr));

            graph
                .active_nodes
                .set(graph.active_nodes.get().saturating_sub(1));
            graph.last_deleted_token.set(Some(deleted_token));
            graph.inc_free_succeeded();
            true
        }
    }
}

use node_store as ag;

use std::fmt;
use std::iter::Iterator;

#[derive(PartialEq, Clone, Copy)]
pub struct NodeGuard<'gg>(ag::NodeGuard<'gg, Node>);

/// slotmap GC 计数快照，用于日志与观测。
#[derive(Debug, Clone, Copy, Default)]
pub struct GcStatsSnapshot {
    pub gc_skipped: u64,
    pub free_skip: u64,
    pub free_attempts: u64,
    pub free_succeeded: u64,
    pub pending_free: usize,
    /// epoch 嵌套深度（>0 表示处于“读窗口”，禁止 token 回收/复用）。
    pub epoch_depth: usize,
    /// epoch 期间被 retire 的待回收节点数量（epoch end 统一尝试回收）。
    pub retired_free: usize,
}

impl GcStatsSnapshot {
    /// 计算 gc_skipped/free_skip 与 pending_free 的总占比，单位千分比。
    #[inline]
    pub fn loss_ppm(&self) -> u64 {
        let attempts = self.free_attempts.max(1);
        let skipped = self
            .gc_skipped
            .saturating_add(self.free_skip)
            .saturating_add(self.pending_free as u64);
        skipped.saturating_mul(1000) / attempts
    }
}

type NodePtr = ag::NodePtr<Node>;

impl<'gg> fmt::Debug for NodeGuard<'gg> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NodeGuard")
            .field("ptr", &self.make_ptr())
            .field("slot_token", &self.slot_token.get())
            .finish()
    }
}

type AgGuard<'gg> = ag::NodeStoreGuard<'gg, Node>;

#[derive(Default, Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum RecalcState {
    #[default]
    Needed,
    Pending,
    Ready,
}

pub struct Graph2 {
    nodes: ag::NodeStore<Node>,
    ////////////////////////////////////////////////////////////////////////////////
    // Anchor 存储池（raw memory blocks）：
    // - mount 时 alloc 一块 Layout 匹配的内存，并原地构造 anchor impl；
    // - reclaim 时 drop_in_place + recycle 内存块，避免每次都走系统分配器。
    ////////////////////////////////////////////////////////////////////////////////
    anchor_pool: AnchorMemPool,
    active_nodes: Cell<usize>,
    pending_free: RefCell<Vec<NodePtr>>,
    ////////////////////////////////////////////////////////////////////////////////
    // Epoch（读窗口）：
    // - epoch_depth > 0：禁止 token 物理回收/复用（nodes.remove -> free list）。
    // - 仅允许把待回收节点“退休(retire)”到队列中，等 epoch end 再统一回收。
    //
    // 这样可以覆盖 “事件派发 + stabilize + render” 的读窗口，避免 TOCTOU：
    // 派发前半段拿到的 token，在派发后半段继续 get 时被回收导致 hard panic。
    ////////////////////////////////////////////////////////////////////////////////
    epoch_depth: Cell<usize>,
    retired_free: RefCell<Vec<NodePtr>>,
    epoch_flushes: Cell<u64>,
    gc_skipped: Cell<u64>,
    free_skip: Cell<u64>,
    free_attempts: Cell<u64>,
    free_succeeded: Cell<u64>,
    token_counter: Cell<u64>,
    last_deleted_token: Cell<Option<u64>>,

    still_alive: Rc<Cell<bool>>,

    /// height -> first node in that height's queue
    recalc_queues: RefCell<Vec<Option<NodePtr>>>,
    recalc_min_height: Cell<usize>,
    recalc_max_height: Cell<usize>,

    /// pointer to head of linked list of free nodes
    free_head: Box<Cell<Option<NodePtr>>>,
}

/// epoch 的 RAII 守卫：
/// - 创建时：epoch_depth += 1
/// - Drop 时：epoch_depth -= 1；当归零时触发 flush_retired（真正回收/复用 token）
pub struct EpochGuard {
    graph: Rc<Graph2>,
}

impl EpochGuard {
    #[must_use]
    pub(crate) fn new(graph: Rc<Graph2>) -> Self {
        graph.enter_epoch();
        Self { graph }
    }
}

impl Drop for EpochGuard {
    fn drop(&mut self) {
        self.graph.leave_epoch();
    }
}

#[derive(Clone, Copy)]
pub struct Graph2Guard<'gg> {
    _nodes: AgGuard<'gg>,
    graph: &'gg Graph2,
}

pub struct Node {
    pub observed: Cell<bool>,

    /// bool used during height incrementing to check for loops
    pub visited: Cell<bool>,

    /// number of nodes that list this node as a necessary child
    pub necessary_count: Cell<usize>,

    pub slot_token: Cell<u64>,
    /// parent 依赖 epoch：
    /// - 用于“逻辑失效”旧 parent link，而不是每次 dirty 都物理清空容器；
    /// - 当前 epoch 的 link 视为 active，旧 epoch 自动视为 stale。
    pub parent_epoch: Cell<u64>,

    pub debug_info: Cell<AnchorDebugInfo>,

    /// tracks the generation when this Node last polled as Updated or Unchanged
    pub(super) last_ready: Cell<Option<Generation>>,
    /// tracks the generation when this Node last polled as Updated
    pub(super) last_update: Cell<Option<Generation>>,
    /// 记录因借用冲突导致的重算重试次数，避免无限排队。
    pub recalc_retry: Cell<u8>,
    /// 累积 deferred dirty 的子节点 token，借用冲突恢复后统一处理。
    pub pending_dirty: RefCell<Vec<NodeKey>>,
    /// 锁期间被请求重算的标记，解锁时统一补偿入队，避免抢锁重入又不丢更新。
    pub pending_recalc: Cell<bool>,

    /// Some() if this node is still active, None otherwise
    ///
    /// 说明：
    /// - 不再用 `Box<dyn GenericAnchor>` 触发每次 mount 的堆分配；
    /// - 改为保存 pool 分配的 raw block（作为 trait object 指针）；
    /// - reclaim 时由 Graph2 负责 drop_in_place + recycle（对齐 epoch 边界）。
    pub anchor: UnsafeCell<Option<NonNull<dyn GenericAnchor>>>,
    /// 标记当前 anchor 是否正被 poll，避免重入。
    pub anchor_locked: Cell<bool>,

    pub ptrs: NodePtrs,
}

impl Drop for Node {
    fn drop(&mut self) {
        ////////////////////////////////////////////////////////////////////////////////
        // Graph2 drop 阶段兜底清理：
        // - 运行期常规回收在 NodeStore::remove（reclaim）里完成；
        // - 但若 Engine/Graph2 被提前 Drop（例如测试提前退出），仍需释放仍然存活的 anchor 存储；
        // - 否则 raw pointer 形态会泄漏（旧的 Box 形态由字段 drop 自动回收）。
        ////////////////////////////////////////////////////////////////////////////////
        let Some(anchor) = self.anchor.get_mut().take() else {
            return;
        };

        // SAFETY:
        // - NodePtrs.graph 指向创建该节点的 Graph2，生命周期覆盖到 Graph2::drop 结束；
        // - Graph2::drop 会先把 still_alive 置 false，避免析构链路再触发回收逻辑访问 graph。
        let graph = unsafe { &*self.ptrs.graph };
        graph.drop_and_recycle_anchor_ptr(anchor);
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy, Hash, PartialOrd, Ord)]
pub struct NodeKey {
    pub ptr: NodePtr,
    token: u64,
}

impl !Send for NodeKey {}
impl !Sync for NodeKey {}

impl NodeKey {
    /// 仅供调试使用，返回 slot token 原始值。
    pub fn raw_token(&self) -> u64 {
        self.token
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ParentLink {
    key: NodeKey,
    epoch: u64,
}

pub struct NodePtrs {
    /// 依赖者（parent）列表：
    /// - 优先用两个 inline 槽位承载最常见的 <=2 个 parent，避免频繁 Vec 分配/扩容；
    /// - 当 parent > 2 时，再落到 `clean_parents` Vec（无序，可能重复，但 add_clean_parent 会去重）。
    clean_parent0: Cell<Option<ParentLink>>,
    clean_parent1: Cell<Option<ParentLink>>,
    clean_parents: RefCell<Vec<ParentLink>>,

    graph: *const Graph2,

    /// Next node in recalc linked list for this height.
    /// If this is the last node, None.
    next: Cell<Option<NodePtr>>,
    /// Prev node in recalc linked list for this height.
    /// If this is the head node, None.
    prev: Cell<Option<NodePtr>>,
    /// 该节点当前所在的 recalc bucket（按高度分桶）。
    ///
    /// 设计动机（性能 + 正确性）：
    /// - 过去 dequeue 需要按 `height(node)` 找 bucket；
    /// - 但 `ensure_height_increases/set_min_height` 可能在节点 Pending 后提升 height；
    /// - 若不同时“改桶”，节点会残留在旧 bucket，导致 dequeue 找不到并遗留脏指针；
    /// - 原修复是“扫描所有 bucket 按 NodePtr 摘除”，正确但在高频 remove 下非常慢。
    ///
    /// 现在改为：
    /// - 入队时记录 `recalc_bucket = Some(node_height)`（表示“当前挂在哪个 bucket 链表上”，不要求等于最新 height）；
    /// - dequeue/remove 时优先用 `recalc_bucket` O(1) 命中并摘除；
    /// - 若链表指针/状态不一致，再兜底扫描所有 bucket（极少数路径）。
    recalc_bucket: Cell<Option<usize>>,
    recalc_state: Cell<RecalcState>,

    /// Next node in free list (仅用于 anchors_slotmap)。
    ///
    /// 重要：free list 与 recalc queue 必须使用**不同**的指针字段。
    /// 否则当“已回收节点”意外残留在 recalc 队列里时，recalc_pop_next 会误把 free list 的 next 当成 queue next，
    /// 直接污染 free_head 链表，最终导致“活节点被复用 -> stale token -> invalid_token request”。
    free_next: Cell<Option<NodePtr>>,
    /// Prev node in free list (仅用于 anchors_slotmap)。
    free_prev: Cell<Option<NodePtr>>,

    /// sorted in pointer order
    necessary_children: RefCell<Vec<NodePtr>>,

    height: Cell<usize>,

    handle_count: Cell<usize>,
}

/// Singlethread's implementation of Anchors' `AnchorHandle`, the engine-specific handle that sits inside an `Anchor`.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct AnchorHandle {
    num: NodeKey,
    still_alive: Rc<Cell<bool>>,
}

impl Clone for AnchorHandle {
    fn clone(&self) -> Self {
        if self.still_alive.get() {
            let guard = unsafe { self.num.ptr.lookup_unchecked() };
            if guard.slot_token.get() == self.num.token {
                let count = &guard.ptrs.handle_count;
                count.set(count.get() + 1);
            }
        }
        AnchorHandle {
            num: self.num,
            // num: self.num.clone(),
            still_alive: self.still_alive.clone(),
        }
    }
}

impl Drop for AnchorHandle {
    fn drop(&mut self) {
        if !self.still_alive.get() {
            return;
        }

        let guard = unsafe { self.num.ptr.lookup_unchecked() };
        if guard.slot_token.get() != self.num.token {
            return;
        }
        let count = &guard.ptrs.handle_count;
        let current = count.get();
        if current == 0 {
            return;
        }
        let new_count = current - 1;
        count.set(new_count);
        if new_count == 0 {
            unsafe { free(self.num.ptr) };
        }
    }
}
impl crate::expert::AnchorHandle for AnchorHandle {
    type Token = NodeKey;
    fn token(&self) -> NodeKey {
        self.num
    }
}

impl<'a> std::ops::Deref for NodeGuard<'a> {
    type Target = Node;
    fn deref(&self) -> &Node {
        &self.0
    }
}

impl<'a> NodeGuard<'a> {
    #[inline]
    fn make_ptr(&self) -> NodePtr {
        self.0.make_ptr()
    }

    pub fn key(self) -> NodeKey {
        NodeKey {
            ptr: self.make_ptr(),
            token: self.slot_token.get(),
        }
    }

    #[inline]
    fn current_parent_epoch(&self) -> u64 {
        self.parent_epoch.get()
    }

    #[inline]
    fn bump_parent_epoch(&self) -> u64 {
        let current = self.current_parent_epoch();
        let mut next = current.wrapping_add(1);
        if next == 0 {
            next = 1;
        }
        self.parent_epoch.set(next);
        current
    }

    #[inline]
    fn is_reusable_parent_link(&self, link: ParentLink, current_epoch: u64) -> bool {
        ////////////////////////////////////////////////////////////////////////////////
        // epoch 失效策略：
        // - link.epoch != current_epoch: 说明是上一轮 dirty 之前的旧依赖，可直接复用槽位；
        // - live 校验失败：说明 parent 已失活（token 代际变化/anchor 为空），也可复用。
        ////////////////////////////////////////////////////////////////////////////////
        link.epoch != current_epoch || self.live_parent_from_key(link.key).is_none()
    }

    pub fn add_clean_parent(self, parent: NodeGuard<'a>) {
        let parent_key = parent.key();
        let current_epoch = self.current_parent_epoch();
        let new_link = ParentLink {
            key: parent_key,
            epoch: current_epoch,
        };
        ////////////////////////////////////////////////////////////////////////////////
        // 架构调整：parent link 不再通过“drain 物理删除”失效，而是通过 epoch 逻辑失效。
        //
        // 规则：
        // - 同 ptr 命中：只刷新 token/epoch，不做容器改动；
        // - 槽位可复用（旧 epoch 或失活）时直接覆盖；
        // - Vec 慢路径也优先复用 stale 槽位，避免 churn 下无限膨胀。
        ////////////////////////////////////////////////////////////////////////////////
        if let Some(mut p0) = self.ptrs.clean_parent0.get() {
            if p0.key.ptr == parent_key.ptr {
                if p0.key.token != parent_key.token || p0.epoch != current_epoch {
                    p0.key = parent_key;
                    p0.epoch = current_epoch;
                    self.ptrs.clean_parent0.set(Some(p0));
                }
                return;
            }
        }
        if let Some(mut p1) = self.ptrs.clean_parent1.get() {
            if p1.key.ptr == parent_key.ptr {
                if p1.key.token != parent_key.token || p1.epoch != current_epoch {
                    p1.key = parent_key;
                    p1.epoch = current_epoch;
                    self.ptrs.clean_parent1.set(Some(p1));
                }
                return;
            }
        }

        let slot0 = self.ptrs.clean_parent0.get();
        if slot0.is_none()
            || slot0.is_some_and(|link| self.is_reusable_parent_link(link, current_epoch))
        {
            self.ptrs.clean_parent0.set(Some(new_link));
        } else {
            let slot1 = self.ptrs.clean_parent1.get();
            if slot1.is_none()
                || slot1.is_some_and(|link| self.is_reusable_parent_link(link, current_epoch))
            {
                self.ptrs.clean_parent1.set(Some(new_link));
            } else {
                // 慢路径：>2 个 parent 才会走 Vec，且尽量复用 stale 槽位。
                let mut parents = self.ptrs.clean_parents.borrow_mut();
                if let Some(existing) = parents
                    .iter_mut()
                    .find(|link| link.key.ptr == parent_key.ptr)
                {
                    if existing.key.token != parent_key.token || existing.epoch != current_epoch {
                        *existing = new_link;
                    }
                } else if let Some(reuse_idx) = parents
                    .iter()
                    .position(|link| self.is_reusable_parent_link(*link, current_epoch))
                {
                    parents[reuse_idx] = new_link;
                } else {
                    parents.push(new_link);
                }
            }
        }
        if debug_parent_link_enabled() {
            // 调试输出：带上节点的 debug_info，便于定位父子是谁
            let child_dbg = self.debug_info.get()._to_string();
            let parent_dbg = parent.debug_info.get()._to_string();
            println!(
                "add_clean_parent child={:?} ({}) parent={:?} ({}) epoch={}",
                self.key().raw_token(),
                child_dbg,
                parent.key().raw_token(),
                parent_dbg,
                current_epoch,
            );
        }
    }

    pub fn remove_clean_parent_ptr(&self, parent_ptr: NodePtr) {
        ////////////////////////////////////////////////////////////////////////////////
        // 解除 child -> parent 的依赖者指针。
        //
        // 为什么要做：
        // - clean_parents 的内容会在 mark_dirty() 时被 drain；
        // - 但如果 parent 被 unrequest/free 且 child 长时间不 dirty，
        //   旧 parent 指针会“卡住”，在 slotmap/free list 复用槽位后变成误伤。
        ////////////////////////////////////////////////////////////////////////////////
        ////////////////////////////////////////////////////////////////////////////////
        // 轻量“压缩”：
        // - 若 clean_parent0 被清空而 clean_parent1 仍有值，则把 clean_parent1 移到 0 槽位，
        //   让后续热路径更容易命中 slot0（减少分支与 Vec 访问概率）。
        ////////////////////////////////////////////////////////////////////////////////
        let mut slot0 = self.ptrs.clean_parent0.get();
        let mut slot1 = self.ptrs.clean_parent1.get();

        if slot0.is_some_and(|link| link.key.ptr == parent_ptr) {
            slot0 = None;
        }
        if slot1.is_some_and(|link| link.key.ptr == parent_ptr) {
            slot1 = None;
        }
        if slot0.is_none() {
            slot0 = slot1;
            slot1 = None;
        }

        self.ptrs.clean_parent0.set(slot0);
        self.ptrs.clean_parent1.set(slot1);

        let mut parents = self.ptrs.clean_parents.borrow_mut();
        if !parents.is_empty() {
            parents.retain(|link| link.key.ptr != parent_ptr);
        }
    }

    #[inline]
    fn live_parent_from_key(&self, parent_key: NodeKey) -> Option<NodeGuard<'a>> {
        ////////////////////////////////////////////////////////////////////////////////
        // 关键：clean_parents 存的是 NodeKey（包含 token），必须校验 token 才能避免：
        // - 节点 free 后进入 free list
        // - 槽位被复用后 slot_token 增长
        // - child 仍持有旧 parent 指针，导致 dirty 误投递到新节点
        ////////////////////////////////////////////////////////////////////////////////
        let parent = NodeGuard(unsafe { parent_key.ptr.lookup_unchecked() });
        if parent.slot_token.get() != parent_key.token {
            return None;
        }
        // anchor 已置空的节点不应再参与 dirty 传播/重算入队。
        if unsafe { (*parent.anchor.get()).is_none() } {
            return None;
        }
        Some(parent)
    }

    pub fn clean_parents(&self) -> Vec<NodeGuard<'a>> {
        let current_epoch = self.current_parent_epoch();
        let mut out = Vec::new();
        if let Some(p0) = self.ptrs.clean_parent0.get() {
            if p0.epoch == current_epoch
                && let Some(parent) = self.live_parent_from_key(p0.key)
            {
                out.push(parent);
            }
        }
        if let Some(p1) = self.ptrs.clean_parent1.get() {
            if p1.epoch == current_epoch
                && let Some(parent) = self.live_parent_from_key(p1.key)
            {
                out.push(parent);
            }
        }
        let parents = self.ptrs.clean_parents.borrow();
        out.extend(parents.iter().filter_map(|link| {
            if link.epoch != current_epoch {
                return None;
            }
            self.live_parent_from_key(link.key)
        }));
        if debug_parent_flow_enabled() {
            println!(
                "CLEAN_PARENTS node={:?} count={}",
                self.debug_info.get()._to_string(),
                out.len()
            );
        }
        out
    }

    /// 遍历父节点，不分配临时 Vec，在线性链路上更轻量。
    pub fn for_each_parent(&self, mut f: impl FnMut(NodeGuard<'a>)) {
        let current_epoch = self.current_parent_epoch();
        if let Some(p0) = self.ptrs.clean_parent0.get() {
            if p0.epoch == current_epoch
                && let Some(parent) = self.live_parent_from_key(p0.key)
            {
                f(parent);
            }
        }
        if let Some(p1) = self.ptrs.clean_parent1.get() {
            if p1.epoch == current_epoch
                && let Some(parent) = self.live_parent_from_key(p1.key)
            {
                f(parent);
            }
        }
        for link in self.ptrs.clean_parents.borrow().iter().copied() {
            if link.epoch != current_epoch {
                continue;
            }
            if let Some(parent) = self.live_parent_from_key(link.key) {
                f(parent);
            }
        }
    }

    /// 遍历“当前 epoch 下”的父节点 key（不做活性校验）。
    ///
    /// 适用场景：
    /// - `set_min_height` 这类“稍后会在处理阶段再次做 token/anchor 校验”的路径；
    /// - 这样可以避免在遍历阶段就做一次 `lookup_unchecked + token/anchor` 检查，
    ///   进而减少重复校验造成的常数开销。
    pub fn for_each_parent_key(&self, mut f: impl FnMut(NodeKey)) {
        let current_epoch = self.current_parent_epoch();
        if let Some(p0) = self.ptrs.clean_parent0.get() {
            if p0.epoch == current_epoch {
                f(p0.key);
            }
        }
        if let Some(p1) = self.ptrs.clean_parent1.get() {
            if p1.epoch == current_epoch {
                f(p1.key);
            }
        }
        for link in self.ptrs.clean_parents.borrow().iter().copied() {
            if link.epoch == current_epoch {
                f(link.key);
            }
        }
    }

    /// 取出并“逻辑失效”当前父节点列表，供 mark_dirty 等热路径复用。
    pub fn drain_parents(&self, mut f: impl FnMut(NodeGuard<'a>)) {
        let draining_epoch = self.bump_parent_epoch();
        if let Some(p0) = self.ptrs.clean_parent0.get() {
            if p0.epoch == draining_epoch
                && let Some(parent) = self.live_parent_from_key(p0.key)
            {
                f(parent);
            }
        }
        if let Some(p1) = self.ptrs.clean_parent1.get() {
            if p1.epoch == draining_epoch
                && let Some(parent) = self.live_parent_from_key(p1.key)
            {
                f(parent);
            }
        }
        for link in self.ptrs.clean_parents.borrow().iter().copied() {
            if link.epoch != draining_epoch {
                continue;
            }
            if let Some(parent) = self.live_parent_from_key(link.key) {
                f(parent);
            }
        }
    }

    /// 父节点数量，用于日志统计，避免 Vec 分配。
    pub fn parents_len(&self) -> usize {
        let current_epoch = self.current_parent_epoch();
        let extra = self
            .ptrs
            .clean_parents
            .borrow()
            .iter()
            .filter(|link| link.epoch == current_epoch)
            .count();
        extra
            + usize::from(
                self.ptrs
                    .clean_parent0
                    .get()
                    .is_some_and(|link| link.epoch == current_epoch),
            )
            + usize::from(
                self.ptrs
                    .clean_parent1
                    .get()
                    .is_some_and(|link| link.epoch == current_epoch),
            )
    }

    pub fn drain_clean_parents(&self) -> Vec<NodeGuard<'a>> {
        let res = self.clean_parents();
        let _ = self.bump_parent_epoch();
        if debug_parent_flow_enabled() {
            println!(
                "DRAIN_CLEAN_PARENTS node={:?} drained={}",
                self.debug_info.get()._to_string(),
                res.len()
            );
        }
        res
    }

    pub fn add_necessary_child(self, child: NodeGuard<'a>) {
        let mut necessary_children = self.ptrs.necessary_children.borrow_mut();
        let child_ptr = child.make_ptr();
        if let Err(i) = necessary_children.binary_search(&child_ptr) {
            necessary_children.insert(i, child_ptr);
            child.necessary_count.set(child.necessary_count.get() + 1)
        }
    }

    pub fn remove_necessary_child(&self, child: NodeGuard<'a>) {
        let mut necessary_children = self.ptrs.necessary_children.borrow_mut();
        let child_ptr = child.make_ptr();
        if let Ok(i) = necessary_children.binary_search(&child_ptr) {
            necessary_children.remove(i);
            child
                .necessary_count
                .set(child.necessary_count.get().saturating_sub(1));

            // 同步解绑 child -> parent，避免产生“只删了一半边”的悬挂 parent 指针。
            child.remove_clean_parent_ptr(self.make_ptr());
        }
    }

    pub fn necessary_children(&self) -> Vec<NodeGuard<'a>> {
        let children = self.ptrs.necessary_children.borrow();
        children
            .iter()
            .map(|ptr| NodeGuard(unsafe { ptr.lookup_unchecked() }))
            .collect()
    }

    pub fn drain_necessary_children(&self) -> Vec<NodeGuard<'a>> {
        let parent_ptr = self.make_ptr();
        let mut children = self.ptrs.necessary_children.borrow_mut();
        for child in children.iter() {
            let child_node = unsafe { self.0.lookup_ptr(*child) };

            // 维护必要性计数：与 add_necessary_child 对称。
            child_node
                .necessary_count
                .set(child_node.necessary_count.get().saturating_sub(1));

            // 关键：解绑 child -> parent，避免 free list 复用槽位后误 dirty。
            if child_node
                .ptrs
                .clean_parent0
                .get()
                .is_some_and(|link| link.key.ptr == parent_ptr)
            {
                child_node.ptrs.clean_parent0.set(None);
            }
            if child_node
                .ptrs
                .clean_parent1
                .get()
                .is_some_and(|link| link.key.ptr == parent_ptr)
            {
                child_node.ptrs.clean_parent1.set(None);
            }
            let mut child_parents = child_node.ptrs.clean_parents.borrow_mut();
            if !child_parents.is_empty() {
                child_parents.retain(|link| link.key.ptr != parent_ptr);
            }
        }
        let collected = children
            .iter()
            .map(|ptr| NodeGuard(unsafe { ptr.lookup_unchecked() }))
            .collect();
        children.clear();
        collected
    }

    #[inline]
    pub fn handle_count(&self) -> usize {
        self.ptrs.handle_count.get()
    }
}

impl<'gg> Graph2Guard<'gg> {
    pub fn active_nodes(&self) -> usize {
        self.graph.active_nodes.get()
    }

    pub fn gc_stats(&self) -> GcStatsSnapshot {
        self.graph.gc_stats_snapshot()
    }

    #[inline]
    pub fn has_recalc_pending(&self) -> bool {
        let queues = self.graph.recalc_queues.borrow();
        let max_height = self.graph.recalc_max_height.get();
        let mut idx = self.graph.recalc_min_height.get();
        if queues.is_empty() || idx > max_height {
            return false;
        }
        idx = idx.min(queues.len().saturating_sub(1));
        while idx <= max_height && idx < queues.len() {
            if queues[idx].is_some() {
                return true;
            }
            idx = idx.saturating_add(1);
        }
        false
    }

    #[inline]
    pub fn has_pending_free(&self) -> bool {
        !self.graph.pending_free.borrow().is_empty()
    }

    pub fn retry_pending_free(&self) {
        self.graph.retry_pending_free();
    }

    pub fn get(&self, key: NodeKey) -> Option<NodeGuard<'gg>> {
        let node = NodeGuard(unsafe { key.ptr.lookup_unchecked() });
        if key.token != node.slot_token.get() {
            return None;
        }
        unsafe {
            if (*node.anchor.get()).is_none() {
                return None;
            }
        }
        Some(node)
    }

    #[cfg(test)]
    pub fn insert_testing_guard(&self) -> NodeGuard<'gg> {
        let handle = self.graph.insert_testing();
        let guard = self.get(handle.num).unwrap();
        std::mem::forget(handle);
        guard
    }

    pub fn recalc_pop_next(&self) -> Option<(usize, NodeGuard<'gg>)> {
        let mut recalc_queues = self.graph.recalc_queues.borrow_mut();
        while self.graph.recalc_min_height.get() <= self.graph.recalc_max_height.get() {
            if let Some(ptr) = recalc_queues[self.graph.recalc_min_height.get()] {
                let node = NodeGuard(unsafe { ptr.lookup_unchecked() });
                recalc_queues[self.graph.recalc_min_height.get()] = node.ptrs.next.get();
                if let Some(next_in_queue_ptr) = node.ptrs.next.get() {
                    NodeGuard(unsafe { next_in_queue_ptr.lookup_unchecked() })
                        .ptrs
                        .prev
                        .set(None);
                }
                node.ptrs.prev.set(None);
                node.ptrs.next.set(None);
                node.ptrs.recalc_bucket.set(None);
                node.ptrs.recalc_state.set(RecalcState::Ready);
                // ┌─────────────────────────────────────────────────────────────┐
                // │ 队列可能残留“已回收节点”（anchor 已置空、handle_count 归零）。│
                // │ 这些节点若继续出队将触发重算 panic，故在此直接跳过。           │
                // └─────────────────────────────────────────────────────────────┘
                if unsafe { (*node.anchor.get()).is_none() } {
                    if debug_queue_enabled() {
                        println!(
                            "QUEUE POP skip freed token={:?} debug={}",
                            node.key().raw_token(),
                            node.debug_info.get()._to_string()
                        );
                    }
                    if cfg!(debug_assertions) {
                        // 理论上 anchor 置空时 handle_count 也应归零；若未归零则说明存在异常释放路径。
                        debug_assert!(
                            node.handle_count() == 0,
                            "queue_pop: anchor=None 但 handle_count>0，疑似非法释放路径，token={:?} debug={}",
                            node.key().raw_token(),
                            node.debug_info.get()._to_string()
                        );
                    }
                    continue;
                }
                if debug_queue_enabled() {
                    println!(
                        "QUEUE POP height={} token={:?} debug={}",
                        self.graph.recalc_min_height.get(),
                        node.key().raw_token(),
                        node.debug_info.get()._to_string()
                    );
                }
                return Some((self.graph.recalc_min_height.get(), node));
            } else {
                self.graph
                    .recalc_min_height
                    .set(self.graph.recalc_min_height.get() + 1);
            }
        }
        ////////////////////////////////////////////////////////////////////////////////
        // NOTE: 队列已耗尽时，恢复到 Graph2::new() 的“空队列哨兵”语义：
        // - recalc_min_height = recalc_queues.len()
        // - recalc_max_height = 0
        //
        // 这样下一次入队时：
        // - 无论 node_height 多大，都能立刻把 min_height 拉回正确起点
        // - 且更配合 queue_recalc() 里对 Pending 丢队列的“min_height > node_height”判断
        ////////////////////////////////////////////////////////////////////////////////
        self.graph.recalc_min_height.set(recalc_queues.len());
        self.graph.recalc_max_height.set(0);
        None
    }

    pub fn queue_recalc(&self, node: NodeGuard<'gg>) {
        // ─────────────────────────────────────────────────────────────
        // 防御性修复：Pending 但可能已“丢队列”
        //
        // 现象：
        // - 某些场景下（例如 Dyn/ForEach 高频增删导致依赖链频繁重排），节点可能处于 `Pending`，
        //   但实际已不在 recalc 队列中（prev/next/queue head 不一致）。
        // - 此时若继续按 “already pending -> skip” 处理，会导致该节点永远不会被 pop，
        //   上游节点不断 request 它并自旋，最终表现为 CPU 满载卡死。
        //
        // 处理：
        // - 当检测到 `recalc_min_height` 已经越过该节点高度时，说明它极可能在队列中“饥饿/丢失”；此时强制重排队。
        // - 这条路径应当非常少见；正常情况下 `Pending` 节点会被 pop，不会进入该分支。
        // ─────────────────────────────────────────────────────────────
        if node.ptrs.recalc_state.get() == RecalcState::Pending {
            // 注意：节点 Pending 后其 height 可能会被提升，但它仍然挂在“入队时”的 bucket 链表上。
            // 因此这里判断“是否可能丢队列/饥饿”应优先用 recalc_bucket，而不是最新 height。
            let node_bucket = node
                .ptrs
                .recalc_bucket
                .get()
                .unwrap_or_else(|| height(node));
            if self.graph.recalc_min_height.get() > node_bucket {
                if debug_queue_enabled() {
                    println!(
                        "queue_recalc requeue pending (min_height={} > node_bucket={}) token={:?} debug={}",
                        self.graph.recalc_min_height.get(),
                        node_bucket,
                        node.key().raw_token(),
                        node.debug_info.get()._to_string()
                    );
                }
                // 若节点确实已丢队列，dequeue_calc 会把它恢复到 Ready；随后走常规入队逻辑。
                dequeue_calc(self.graph, node);
            } else {
                if debug_queue_enabled() {
                    println!(
                        "queue_recalc skip (already pending) token={:?} debug={}",
                        node.key().raw_token(),
                        node.debug_info.get()._to_string()
                    );
                }
                // already in recalc queue
                return;
            }
        }
        // 如果节点正在重算（anchor_locked=true），跳过此次入队，改为标记“需补偿”，由 AnchorLockGuard 在解锁时入队，避免 strict 下的重入同时不丢更新。
        if node.anchor_locked.get() {
            #[cfg(debug_assertions)]
            if crate::singlethread::lock_trace_enabled() {
                tracing::warn!(
                    target: "anchors",
                    "queue_recalc skip locked node token={:?} debug={}",
                    node.key().raw_token(),
                    node.debug_info.get()._to_string()
                );
            }
            node.pending_recalc.set(true);
            return;
        }
        if debug_queue_enabled() {
            println!(
                "queue_recalc enqueue token={:?} height={} debug={}",
                node.key().raw_token(),
                height(node),
                node.debug_info.get()._to_string()
            );
        }
        #[cfg(debug_assertions)]
        if crate::singlethread::lock_trace_enabled() {
            use std::backtrace::Backtrace;
            let bt = Backtrace::force_capture();
            tracing::warn!(
                target: "anchors",
                "queue_recalc: token={:?} locked={} height={} debug={}",
                node.key().raw_token(),
                node.anchor_locked.get(),
                height(node),
                node.debug_info.get()._to_string()
            );
            println!(
                "QUEUE_RECALC TRACE token={:?}\n{:?}",
                node.key().raw_token(),
                bt
            );
        }
        node.ptrs.recalc_state.set(RecalcState::Pending);
        let node_height = height(node);
        let mut recalc_queues = self.graph.recalc_queues.borrow_mut();
        if node_height >= recalc_queues.len() {
            let old_len = recalc_queues.len();
            // ── 以 1.2 倍扩容减少每次只增加一个槽位导致的高频 warn，同时仍确保覆盖必需高度。
            let required_len = node_height.saturating_add(1);
            let ratio_len = {
                let scaled = old_len.saturating_mul(12).saturating_add(9) / 10;
                scaled.max(old_len.saturating_add(1))
            };
            let target_len = required_len.max(ratio_len);
            recalc_queues.resize(target_len, None);
            if self.graph.recalc_min_height.get() >= old_len {
                // 队列原本为空（min==len），扩容后同步更新哨兵值，确保后续 pop 逻辑不越界。
                self.graph.recalc_min_height.set(target_len);
            }
            tracing::warn!(
                target: "anchors",
                token = node.key().raw_token(),
                old_capacity = old_len,
                new_capacity = target_len,
                ratio_target = ratio_len,
                required_height = node_height,
                debug = %node.debug_info.get()._to_string(),
                "recalc_queues 容量不足，自动扩容"
            );
        }
        if let Some(old) = recalc_queues[node_height] {
            NodeGuard(unsafe { old.lookup_unchecked() })
                .ptrs
                .prev
                .set(Some(node.make_ptr()));
            node.ptrs.next.set(Some(old));
        } else {
            if self.graph.recalc_min_height.get() > node_height {
                self.graph.recalc_min_height.set(node_height);
            }
            if self.graph.recalc_max_height.get() < node_height {
                self.graph.recalc_max_height.set(node_height);
            }
        }
        node.ptrs.recalc_bucket.set(Some(node_height));
        recalc_queues[node_height] = Some(node.make_ptr());
    }

    /// 强制入队：无视 Pending/locked 状态，先摘旧队列再入队（主要用于解锁后补偿重算）。
    pub fn queue_recalc_force(&self, node: NodeGuard<'gg>) {
        // 若已挂在 Pending 队列，先摘除再重排。
        if node.ptrs.recalc_state.get() == RecalcState::Pending {
            dequeue_calc(self.graph, node);
            node.ptrs.recalc_state.set(RecalcState::Ready);
        }
        // 解锁后走常规入队逻辑，避免重复维护 prev/next。
        node.anchor_locked.set(false);
        self.queue_recalc(node);
    }

    /// 读取已 Ready 节点的输出副本，不触发额外 request。
    pub fn output_cached<O: Clone + 'static>(&self, engine: &Engine, node: NodeGuard<'gg>) -> O {
        let anchor_ptr = unsafe {
            (*node.anchor.get()).unwrap_or_else(|| {
                panic!(
                    "slotmap: anchor 缺失，无法读取缓存输出 token={:?}",
                    node.key()
                )
            })
        };
        let anchor_impl = unsafe { anchor_ptr.as_ref() };
        anchor_impl
            .output(&mut EngineContext { engine })
            .downcast_ref::<O>()
            .expect("slotmap: output_cached 类型不匹配")
            .clone()
    }
}

impl Graph2 {
    fn next_slot_token(&self) -> u64 {
        let current = self.token_counter.get();
        self.token_counter.set(current + 1);
        current
    }

    pub fn token_counter(&self) -> u64 {
        self.token_counter.get()
    }

    pub fn last_deleted_token(&self) -> Option<u64> {
        self.last_deleted_token.get()
    }

    #[inline]
    fn inc_gc_skipped(&self) {
        self.gc_skipped.set(self.gc_skipped.get().saturating_add(1));
    }

    #[inline]
    fn inc_free_skip(&self) {
        self.free_skip.set(self.free_skip.get().saturating_add(1));
    }

    #[inline]
    fn inc_free_attempt(&self) {
        self.free_attempts
            .set(self.free_attempts.get().saturating_add(1));
    }

    #[inline]
    fn inc_free_succeeded(&self) {
        self.free_succeeded
            .set(self.free_succeeded.get().saturating_add(1));
    }

    pub fn gc_stats_snapshot(&self) -> GcStatsSnapshot {
        GcStatsSnapshot {
            gc_skipped: self.gc_skipped.get(),
            free_skip: self.free_skip.get(),
            free_attempts: self.free_attempts.get(),
            free_succeeded: self.free_succeeded.get(),
            pending_free: self.pending_free.borrow().len(),
            epoch_depth: self.epoch_depth.get(),
            retired_free: self.retired_free.borrow().len(),
        }
    }

    #[must_use]
    pub fn anchor_pool_stats_snapshot(&self) -> AnchorPoolStatsSnapshot {
        self.anchor_pool.stats_snapshot()
    }

    fn enqueue_free_retry(&self, ptr: NodePtr) {
        let mut pending = self.pending_free.borrow_mut();
        if !pending.iter().any(|p| *p == ptr) {
            pending.push(ptr);
        }
    }

    fn retry_pending_free(&self) {
        ////////////////////////////////////////////////////////////////////////////////
        // epoch 内禁止 token 物理回收/复用：
        // - pending_free 里的节点同样属于“待回收”，必须等到 epoch end 才能真正 remove。
        ////////////////////////////////////////////////////////////////////////////////
        if self.epoch_depth.get() > 0 {
            return;
        }

        let mut pending = self.pending_free.borrow_mut();
        if pending.is_empty() {
            return;
        }

        let mut still_pending = Vec::new();
        for ptr in pending.drain(..) {
            debug_assert_eq!(
                self.epoch_depth.get(),
                0,
                "slotmap: retry_pending_free 不应在 epoch 内触发 remove"
            );
            let freed = unsafe { self.nodes.remove(ptr) };
            if !freed {
                if !still_pending.iter().any(|p| *p == ptr) {
                    still_pending.push(ptr);
                }
            }
        }
        *pending = still_pending;
    }

    ////////////////////////////////////////////////////////////////////////////////
    // Epoch（读窗口）实现
    ////////////////////////////////////////////////////////////////////////////////
    fn epoch_trace_enabled() -> bool {
        static ENABLED: OnceLock<bool> = OnceLock::new();
        *ENABLED.get_or_init(|| emg_debug_env::bool_strict("ANCHORS_EPOCH_TRACE"))
    }

    fn enter_epoch(&self) {
        let prev = self.epoch_depth.get();
        let next = prev.saturating_add(1);
        self.epoch_depth.set(next);

        // 只在 0->1 时打印 begin，避免嵌套时刷屏。
        if prev == 0 && Self::epoch_trace_enabled() {
            let retired_len = self.retired_free.borrow().len();
            let pending_len = self.pending_free.borrow().len();
            eprintln!(
                "[anchors][epoch] begin depth={next} retired_free={retired_len} pending_free={pending_len}"
            );
        }
    }

    fn leave_epoch(&self) {
        let prev = self.epoch_depth.get();
        debug_assert!(prev > 0, "slotmap: leave_epoch depth 不应为 0");
        if prev == 0 {
            return;
        }

        let next = prev - 1;
        self.epoch_depth.set(next);

        // 只在 1->0 时打印 end，并触发真正回收。
        if next == 0 {
            if Self::epoch_trace_enabled() {
                let retired_len = self.retired_free.borrow().len();
                let pending_len = self.pending_free.borrow().len();
                eprintln!(
                    "[anchors][epoch] end depth={next} retired_free={retired_len} pending_free={pending_len}"
                );
            }
            self.flush_retired();
        }
    }

    fn retire_free(&self, ptr: NodePtr) {
        let mut retired = self.retired_free.borrow_mut();
        if retired.iter().any(|p| *p == ptr) {
            return;
        }
        retired.push(ptr);

        if Self::epoch_trace_enabled() {
            // NOTE: 这里不打印 token/debug，避免和 free_trace_cfg 的定位能力重复。
            //       epoch trace 更关注“队列是否积压”。
            let depth = self.epoch_depth.get();
            let retired_len = retired.len();
            eprintln!("[anchors][epoch] retire depth={depth} retired_free={retired_len}");
        }
    }

    fn flush_retired(&self) {
        debug_assert_eq!(
            self.epoch_depth.get(),
            0,
            "slotmap: flush_retired 只能在 epoch end 执行"
        );

        let mut retired = self.retired_free.borrow_mut();
        if retired.is_empty() {
            drop(retired);
            // 仍然尝试清理 pending_free，避免残留。
            self.retry_pending_free();
            return;
        }

        self.epoch_flushes
            .set(self.epoch_flushes.get().saturating_add(1));

        // 将 retire 列表一次性取出，避免在 remove 时持有 RefCell 借用。
        let to_free: Vec<NodePtr> = retired.drain(..).collect();
        drop(retired);

        for ptr in to_free {
            debug_assert_eq!(
                self.epoch_depth.get(),
                0,
                "slotmap: flush_retired 不应在 epoch 内触发 remove"
            );

            let freed = unsafe { self.nodes.remove(ptr) };
            if !freed {
                self.enqueue_free_retry(ptr);
            }
        }

        // 统一在 epoch end 再尝试一次 pending_free，最大化回收成功率。
        self.retry_pending_free();
    }

    pub fn new(max_height: usize) -> Self {
        Self {
            nodes: ag::NodeStore::new(),
            anchor_pool: AnchorMemPool::new(),
            token_counter: Cell::new(0),
            active_nodes: Cell::new(0),
            pending_free: RefCell::new(vec![]),
            epoch_depth: Cell::new(0),
            retired_free: RefCell::new(vec![]),
            epoch_flushes: Cell::new(0),
            gc_skipped: Cell::new(0),
            free_skip: Cell::new(0),
            free_attempts: Cell::new(0),
            free_succeeded: Cell::new(0),
            last_deleted_token: Cell::new(None),
            recalc_queues: RefCell::new(vec![None; max_height]),
            recalc_min_height: Cell::new(max_height),
            recalc_max_height: Cell::new(0),
            still_alive: Rc::new(Cell::new(true)),
            free_head: Box::new(Cell::new(None)),
        }
    }

    pub fn with<F: for<'any> FnOnce(Graph2Guard<'any>) -> R, R>(&self, func: F) -> R {
        let nodes = unsafe { self.nodes.with_unchecked() };
        func(Graph2Guard {
            _nodes: nodes,
            graph: self,
        })
    }

    ////////////////////////////////////////////////////////////////////////////////
    // Anchor storage: pool alloc + 原地构造/析构
    ////////////////////////////////////////////////////////////////////////////////

    #[inline]
    fn alloc_anchor_ptr<I>(&self, inner: I) -> NonNull<dyn GenericAnchor>
    where
        I: GenericAnchor + 'static,
    {
        let layout = std::alloc::Layout::new::<I>();
        let raw = self.anchor_pool.alloc(layout);
        let typed: NonNull<I> = raw.cast();
        unsafe {
            // 原地构造：避免 `Box::new` 的堆分配热点。
            typed.as_ptr().write(inner);
        }

        // 说明：
        // - raw pointer 赋值支持 unsize coercion：*mut I -> *mut dyn GenericAnchor
        // - NonNull 要求非空；pool.alloc 已保证返回非空指针（含 ZST 虚指针）。
        let trait_ptr: *mut dyn GenericAnchor = typed.as_ptr();
        unsafe { NonNull::new_unchecked(trait_ptr) }
    }

    #[inline]
    fn drop_and_recycle_anchor_ptr(&self, anchor: NonNull<dyn GenericAnchor>) {
        unsafe {
            // dyn trait object：size/align 由 vtable 元数据决定。
            let anchor_ref: &dyn GenericAnchor = anchor.as_ref();
            let size = std::mem::size_of_val(anchor_ref);
            let align = std::mem::align_of_val(anchor_ref);
            let layout = std::alloc::Layout::from_size_align(size, align)
                .expect("anchor pool: size/align 应构成合法 Layout");

            // 先析构对象，再回收 raw block。
            std::ptr::drop_in_place(anchor.as_ptr());

            // 将 fat pointer 降为 data pointer（thin），交给 pool 复用。
            let data_ptr = anchor.as_ptr() as *mut () as *mut u8;
            let data = NonNull::new_unchecked(data_ptr);
            self.anchor_pool.recycle(data, layout);
        }
    }

    #[cfg(test)]
    pub fn insert_testing(&self) -> AnchorHandle {
        self.insert(
            crate::expert::constant::Constant::new_raw_testing(123),
            AnchorDebugInfo {
                location: None,
                type_info: "testing dummy anchor",
            },
        )
    }

    pub(super) fn insert<I>(&'_ self, inner: I, debug_info: AnchorDebugInfo) -> AnchorHandle
    where
        I: GenericAnchor + 'static,
    {
        let anchor = self.alloc_anchor_ptr(inner);
        self.nodes.with(|nodes| {
            let guard = if let Some(free_head) = self.free_head.get() {
                let guard = NodeGuard(unsafe { free_head.lookup_unchecked() });
                // 从 free list 取出一个空槽位。
                //
                // anchors_slotmap 下：free list 使用专用指针字段（free_next/free_prev），
                // 避免和 recalc 队列（next/prev）互相污染导致“活节点被误复用”。
                {
                    self.free_head.set(guard.ptrs.free_next.get());
                    if let Some(next_ptr) = guard.ptrs.free_next.get() {
                        let next_node = unsafe { nodes.lookup_ptr(next_ptr) };
                        next_node.ptrs.free_prev.set(None);
                    }
                }
                guard.observed.set(false);
                guard.visited.set(false);
                guard.necessary_count.set(0);
                ////////////////////////////////////////////////////////////////////////////////
                // NOTE:
                // - 纯调试能力：定位“哪个旧 token 被复用成新 token”，用于追踪失效 token 的来源。
                // - 触发点：Graph2::insert 复用 free list 槽位时。
                //
                // 用法（示例）：
                // - ANCHORS_TRACE_REUSE=1 ANCHORS_TRACE_REUSE_TOKEN=1012 ANCHORS_TRACE_REUSE_BACKTRACE=1
                // - ANCHORS_TRACE_REUSE=1 ANCHORS_TRACE_REUSE_MATCH=node_builder.rs
                ////////////////////////////////////////////////////////////////////////////////
                {
                    let cfg = reuse_trace_cfg();
                    if cfg.enabled {
                        let old_token = guard.slot_token.get();
                        let old_debug = guard.debug_info.get()._to_string();
                        if cfg.should_log(old_token, old_debug.as_str()) {
                            let new_token = self.next_slot_token();
                            let new_debug = debug_info._to_string();
                            ////////////////////////////////////////////////////////////////////////////////
                            // NOTE:
                            // - 复用时补充“旧槽位状态”用于定位 invalid_token：
                            //   - old_handle_count：理论上应为 0（已进入 free list）。
                            //   - old_necessary_count：理论上应为 0（已无必要父链）。
                            //   - old_anchor_none：理论上应为 true（anchor 已被 remove）。
                            //
                            // 若这些值不满足预期，说明存在“仍被引用但被回收/复用”的生命周期错误。
                            ////////////////////////////////////////////////////////////////////////////////
                            let old_handle_count = guard.ptrs.handle_count.get();
                            let old_necessary_count = guard.necessary_count.get();
                            let old_anchor_none = unsafe { (&*guard.anchor.get()).is_none() };
                            eprintln!(
                                "[anchors][reuse] old_token={old_token} old_handle_count={old_handle_count} old_necessary_count={old_necessary_count} old_anchor_none={old_anchor_none} new_token={new_token} old_debug={old_debug} new_debug={new_debug}",
                            );
                            if cfg.backtrace {
                                let bt = Backtrace::force_capture();
                                eprintln!("[anchors][reuse] backtrace:\n{bt}");
                            }
                            guard.slot_token.set(new_token);
                        } else {
                            guard.slot_token.set(self.next_slot_token());
                        }
                    } else {
                        guard.slot_token.set(self.next_slot_token());
                    }
                }
                guard.ptrs.clean_parent0.set(None);
                guard.ptrs.clean_parent1.set(None);
                ////////////////////////////////////////////////////////////////////////////////
                // 复用 Vec 容量,降低动态 churn 下的分配/回收风暴
                //
                // 背景：
                // - 该节点来自 free list,这些 Vec 往往在上一轮生命周期里已经扩容过；
                // - 若这里用 `replace(Vec::new())`,会立刻释放旧 buffer,后续又在 request/match 中重新扩容,
                //   pprof 里常体现为 `RawVecInner::finish_grow` 热点。
                //
                // 风险控制：
                // - 为避免极端情况下 free list “长期持有超大 Vec buffer”导致 RSS 不可控,
                //   对少数超大容量做阈值回收(直接丢弃 buffer)。
                ////////////////////////////////////////////////////////////////////////////////
                {
                    let mut parents = guard.ptrs.clean_parents.borrow_mut();
                    parents.clear();
                    // clean_parents 通常很小,默认不做 shrink,尽量保留容量复用。
                }
                guard.parent_epoch.set(1);
                guard.ptrs.recalc_bucket.set(None);
                guard.ptrs.recalc_state.set(RecalcState::Needed);
                guard.ptrs.free_next.set(None);
                guard.ptrs.free_prev.set(None);
                {
                    const MAX_KEEP_CAPACITY: usize = 256;
                    let mut children = guard.ptrs.necessary_children.borrow_mut();
                    children.clear();
                    if children.capacity() > MAX_KEEP_CAPACITY {
                        *children = Vec::new();
                    }
                }
                guard.ptrs.height.set(0);
                guard.ptrs.handle_count.set(1);
                guard.ptrs.prev.set(None);
                guard.ptrs.next.set(None);
                guard.debug_info.set(debug_info);
                guard.last_ready.set(None);
                guard.last_update.set(None);
                guard.recalc_retry.set(0);
                {
                    const MAX_KEEP_CAPACITY: usize = 64;
                    let mut pending = guard.pending_dirty.borrow_mut();
                    pending.clear();
                    if pending.capacity() > MAX_KEEP_CAPACITY {
                        *pending = Vec::new();
                    }
                }
                guard.pending_recalc.set(false);
                unsafe {
                    *guard.anchor.get() = Some(anchor);
                }
                guard.anchor_locked.set(false);
                guard
            } else {
                let node = Node {
                    observed: Cell::new(false),
                    visited: Cell::new(false),
                    necessary_count: Cell::new(0),
                    slot_token: Cell::new(self.next_slot_token()),
                    parent_epoch: Cell::new(1),
                    ptrs: NodePtrs {
                        clean_parent0: Cell::new(None),
                        clean_parent1: Cell::new(None),
                        clean_parents: RefCell::new(vec![]),
                        graph: self,
                        next: Cell::new(None),
                        prev: Cell::new(None),
                        recalc_bucket: Cell::new(None),
                        recalc_state: Cell::new(RecalcState::Needed),
                        free_next: Cell::new(None),
                        free_prev: Cell::new(None),
                        necessary_children: RefCell::new(vec![]),
                        height: Cell::new(0),
                        handle_count: Cell::new(1),
                    },
                    debug_info: Cell::new(debug_info),
                    last_ready: Cell::new(None),
                    last_update: Cell::new(None),
                    recalc_retry: Cell::new(0),
                    pending_dirty: RefCell::new(vec![]),
                    pending_recalc: Cell::new(false),
                    anchor: UnsafeCell::new(Some(anchor)),
                    anchor_locked: Cell::new(false),
                };
                NodeGuard(nodes.insert(node))
            };
            let num = guard.key();
            self.active_nodes
                .set(self.active_nodes.get().saturating_add(1));
            AnchorHandle {
                num,
                still_alive: self.still_alive.clone(),
            }
        })
    }
}

impl Drop for Graph2 {
    fn drop(&mut self) {
        self.still_alive.set(false);
    }
}

pub fn ensure_height_increases<'a>(
    child: NodeGuard<'a>,
    parent: NodeGuard<'a>,
) -> Result<bool, ()> {
    if height(child) < height(parent) {
        return Ok(true);
    }
    child.visited.set(true);
    let res = set_min_height(parent, height(child) + 1);
    child.visited.set(false);
    res.map(|()| false)
}

fn set_min_height(node: NodeGuard<'_>, min_height: usize) -> Result<(), ()> {
    // 剪枝：高度已满足，无需向上回填。
    if height(node) >= min_height {
        return Ok(());
    }

    ////////////////////////////////////////////////////////////////////////////////
    // 性能改良：把“递归 + 每层分配 Vec”改为“显式栈 DFS”
    //
    // 背景：
    // - `ensure_height_increases` 会在 request 依赖时调用本函数；
    // - 在深链/高频 request 场景里，递归 + 临时 Vec 分配会产生显著常数开销；
    // - pprof 里 `set_min_height` 会占很高的 flat%。
    //
    // 做法：
    // - 用一个 Vec 作为工作栈，走 DFS 传播；
    // - 仍然沿用 `node.visited` 做环检测；
    // - 发现环时不立刻 return（否则会遗留 visited=true），而是标记 did_err；
    // - 最关键：复用 thread-local 的 Vec 缓冲区，避免每次调用都分配/释放，
    //   同时把 capacity 拉高到一个“足够大”的值，减少 `grow_one/finish_grow` 反复扩容。
    ////////////////////////////////////////////////////////////////////////////////
    #[derive(Clone, Copy)]
    enum WorkKind {
        /// 已经在“入栈阶段”完成活性校验的 Enter（本轮 set_min_height 内可视为稳定）。
        ///
        /// 目的：
        /// - 避免“入栈阶段校验一次 + 出栈阶段再校验一次”的重复常数；
        /// - 在 parent 较多时，这个重复校验会在 pprof 里集中成 Top 热点。
        EnterAssumedLive,
        Exit,
    }

    #[derive(Clone, Copy)]
    struct WorkItem {
        kind: WorkKind,
        key: NodeKey,
        min_height: usize,
    }

    ////////////////////////////////////////////////////////////////////////////////
    // 性能关键：
    // - 这里的 stack 只在本线程使用；
    // - 通过 thread-local 复用底层容量，避免每次调用都触发 alloc/dealloc；
    // - 用 Enter/Exit 两类步骤模拟递归的“入栈/出栈”，保持 visited 的语义为“当前路径栈标记”（用于环检测）。
    ////////////////////////////////////////////////////////////////////////////////
    const MIN_CAPACITY: usize = 256;
    thread_local! {
        static STACK: RefCell<Vec<WorkItem>> = RefCell::new(Vec::new());
    }

    // NOTE:
    // - 仅用于本轮 `set_min_height` 内部（单线程、无 free/reuse 的前提下）；
    // - 调用方必须确保该 key 在“入栈阶段”已通过 token+anchor 校验。
    #[inline]
    unsafe fn node_from_key_unchecked<'a>(key: NodeKey) -> NodeGuard<'a> {
        NodeGuard(unsafe { key.ptr.lookup_unchecked() })
    }

    STACK.with(|stack_cell| {
        let mut stack = stack_cell.borrow_mut();

        // 复用容量：只 clear，不释放。
        stack.clear();

        // 关键：尽量在第一次就把 capacity 拉到一个“够用”的水平，减少 grow_one。
        {
            let cap = stack.capacity();
            if cap < MIN_CAPACITY {
                stack.reserve(MIN_CAPACITY - cap);
            }
        }

        stack.push(WorkItem {
            kind: WorkKind::EnterAssumedLive,
            key: node.key(),
            min_height,
        });

        let mut did_err = false;
        while let Some(item) = stack.pop() {
            match item.kind {
                WorkKind::Exit => {
                    // Exit 一定来自本轮 Enter 成功后的 push，因此这里可以跳过重复活性校验。
                    let n = unsafe { node_from_key_unchecked(item.key) };
                    #[cfg(debug_assertions)]
                    {
                        debug_assert_eq!(
                            n.slot_token.get(),
                            item.key.token,
                            "set_min_height Exit: slot_token/token 不一致（疑似 key 非 live）"
                        );
                        debug_assert!(
                            unsafe { (*n.anchor.get()).is_some() },
                            "set_min_height Exit: anchor=None（疑似 key 非 live）"
                        );
                    }
                    n.visited.set(false);
                }
                WorkKind::EnterAssumedLive => {
                    let n = unsafe { node_from_key_unchecked(item.key) };
                    #[cfg(debug_assertions)]
                    {
                        debug_assert_eq!(
                            n.slot_token.get(),
                            item.key.token,
                            "set_min_height EnterAssumedLive: slot_token/token 不一致（疑似 key 非 live）"
                        );
                        debug_assert!(
                            unsafe { (*n.anchor.get()).is_some() },
                            "set_min_height EnterAssumedLive: anchor=None（疑似 key 非 live）"
                        );
                    }

                    // 剪枝：已有更大的高度，无需继续向上回填。
                    if height(n) >= item.min_height {
                        continue;
                    }
                    // 环检测：只标记错误，依靠 Exit 步骤清理 visited。
                    if n.visited.get() {
                        did_err = true;
                        continue;
                    }

                    n.visited.set(true);
                    n.ptrs.height.set(item.min_height);

                    // 先压 Exit，再压父节点 Enter：保证“先处理父，再清 visited”。
                    stack.push(WorkItem {
                        kind: WorkKind::Exit,
                        key: n.key(),
                        min_height: 0,
                    });

                    let parent_min_h = item.min_height + 1;
                    n.for_each_parent_key(|parent_key| {
                        ////////////////////////////////////////////////////////////////////////////////
                        // 性能：parent 遍历阶段做一次“早剪枝”，减少无效 WorkItem 入栈
                        //
                        // - 之前这里会把所有 parent 都 push 进栈，很多 parent 在后续 pop 时会因为：
                        //   - token/anchor 失效（节点已被 GC/free）
                        //   - height 已满足（无需提升）
                        //   而立刻被剪枝。
                        // - 这些“push + pop + 再剪枝”的纯常数会在 pprof 里集中到该行。
                        // - 这里先做一次轻量判断：只有确实需要提升的 parent 才入栈。
                        //
                        // 注意：依赖环检测仍保持语义：
                        // - 仅当 parent 需要提升且已在当前路径 visited 时，才标记 did_err。
                        ////////////////////////////////////////////////////////////////////////////////
                        // NOTE:
                        // - 先读 height：高度已满足的 parent，直接返回，避免多余的 token/anchor 读取；
                        // - 只有在“确实需要提升”的情况下，才做 token/anchor 校验与 visited 检测。
                        let parent = NodeGuard(unsafe { parent_key.ptr.lookup_unchecked() });
                        if height(parent) >= parent_min_h {
                            return;
                        }
                        if parent.slot_token.get() != parent_key.token {
                            return;
                        }
                        if unsafe { (*parent.anchor.get()).is_none() } {
                            return;
                        }
                        if parent.visited.get() {
                            did_err = true;
                            return;
                        }

                        stack.push(WorkItem {
                            // parent 已通过 token+anchor 校验，这里标记为 AssumedLive，
                            // 避免后续 pop 时再次做 token/anchor 校验。
                            kind: WorkKind::EnterAssumedLive,
                            key: parent_key,
                            min_height: parent_min_h,
                        });
                    });
                }
            }
        }

        if did_err { Err(()) } else { Ok(()) }
    })
}

/// 辅助：当节点已经 Pending 但需要“重新排队”时，先摘下旧位置再重新入队，避免 recalc_min_height 已越过导致饥饿。
#[allow(dead_code)]
pub fn requeue_pending<'gg>(graph: Graph2Guard<'gg>, node: NodeGuard<'gg>) {
    // 仅处理 Pending 节点，其余保持原逻辑。
    if node.ptrs.recalc_state.get() != RecalcState::Pending {
        return;
    }
    // 先摘出旧的队列位置。
    dequeue_calc(graph.graph, node);
    node.ptrs.recalc_state.set(RecalcState::Ready);
    // 再按 queue_recalc 正常入队，保证 recalc_min_height/prev/next 更新。
    graph.queue_recalc(node);
}

/// 将节点状态强制置为 Ready（不改动队列指针），用于锁释放后无条件重新入队。
#[allow(dead_code)]
pub fn set_recalc_ready(node: NodeGuard<'_>) {
    node.ptrs.recalc_state.set(RecalcState::Ready);
}

fn dequeue_calc(graph: &Graph2, node: NodeGuard<'_>) {
    if node.ptrs.recalc_state.get() != RecalcState::Pending {
        return;
    }
    if dequeue_calc_by_ptr(graph, node.make_ptr()) {
        return;
    }

    // 若队列中已找不到该节点，重置状态后返回，避免 panic 连环崩溃。
    if cfg!(debug_assertions) {
        tracing::warn!(
            target: "anchors",
            "dequeue_calc: pending 节点在队列中缺失，直接跳过 token={:?} debug={}",
            node.key().raw_token(),
            node.debug_info.get()._to_string()
        );
    }
    node.ptrs.recalc_state.set(RecalcState::Ready);
    node.ptrs.prev.set(None);
    node.ptrs.next.set(None);
    node.ptrs.recalc_bucket.set(None);
}

fn dequeue_calc_by_ptr(graph: &Graph2, target_ptr: NodePtr) -> bool {
    // ─────────────────────────────────────────────────────────────
    // 关键修复（升级版）：
    // - 过去为了处理“Pending 后 height 被提升导致找不到 bucket”，这里会扫描所有 bucket；
    // - 现在为每个 Pending 节点维护 `recalc_bucket`（即使 height 后续变化也不影响 dequeue 命中）；
    // - 因此绝大多数情况下可以 O(1) 摘除，扫描仅作为极少数兜底。
    // ─────────────────────────────────────────────────────────────
    let target_guard = unsafe { target_ptr.lookup_unchecked() };
    // 这里不能完全信任 recalc_state：
    // - remove/free 路径要求“尽力把节点从队列摘干净”，以免残留脏指针污染 free list；
    // - 在极端重入/借用冲突修复路径里，可能出现 state 与链表指针短暂不一致。
    //
    // 但也不能无脑扫描所有 bucket（remove 会高频触发）。
    // 因此先做一个“是否可能在队列里”的快速判定：
    // - 有 bucket / 有 prev/next / 或 state=Pending 才继续；否则直接返回。
    let has_link = target_guard.ptrs.prev.get().is_some() || target_guard.ptrs.next.get().is_some();
    let bucket_opt = target_guard.ptrs.recalc_bucket.get();
    if bucket_opt.is_none()
        && !has_link
        && target_guard.ptrs.recalc_state.get() != RecalcState::Pending
    {
        return false;
    }

    if let Some(bucket_idx) = bucket_opt {
        let mut recalc_queues = graph.recalc_queues.borrow_mut();
        if bucket_idx < recalc_queues.len() {
            let prev_ptr = target_guard.ptrs.prev.get();
            let next_ptr = target_guard.ptrs.next.get();

            let head_matches = prev_ptr.is_some() || recalc_queues[bucket_idx] == Some(target_ptr);
            let prev_matches = prev_ptr.is_none()
                || unsafe { prev_ptr.unwrap().lookup_unchecked() }
                    .ptrs
                    .next
                    .get()
                    == Some(target_ptr);
            let next_matches = next_ptr.is_none()
                || unsafe { next_ptr.unwrap().lookup_unchecked() }
                    .ptrs
                    .prev
                    .get()
                    == Some(target_ptr);

            if head_matches && prev_matches && next_matches {
                if let Some(prev_ptr) = prev_ptr {
                    unsafe { prev_ptr.lookup_unchecked() }
                        .ptrs
                        .next
                        .set(next_ptr);
                } else {
                    recalc_queues[bucket_idx] = next_ptr;
                }

                if let Some(next_ptr) = next_ptr {
                    unsafe { next_ptr.lookup_unchecked() }
                        .ptrs
                        .prev
                        .set(prev_ptr);
                }

                target_guard.ptrs.prev.set(None);
                target_guard.ptrs.next.set(None);
                target_guard.ptrs.recalc_bucket.set(None);
                target_guard.ptrs.recalc_state.set(RecalcState::Ready);
                return true;
            }
        }
    }

    // ── 兜底：扫描所有 bucket，按 NodePtr 精确摘除目标节点
    let mut recalc_queues = graph.recalc_queues.borrow_mut();
    for queue_idx in 0..recalc_queues.len() {
        let mut head_ptr = recalc_queues[queue_idx];
        let mut found_prev: Option<NodePtr> = None;

        while let Some(cur_ptr) = head_ptr {
            if cur_ptr == target_ptr {
                let cur_guard = unsafe { cur_ptr.lookup_unchecked() };
                let next_ptr = cur_guard.ptrs.next.get();

                if let Some(prev_ptr) = found_prev {
                    unsafe { prev_ptr.lookup_unchecked() }
                        .ptrs
                        .next
                        .set(next_ptr);
                } else {
                    recalc_queues[queue_idx] = next_ptr;
                }

                if let Some(next) = next_ptr {
                    unsafe { next.lookup_unchecked() }.ptrs.prev.set(found_prev);
                }

                cur_guard.ptrs.prev.set(None);
                cur_guard.ptrs.next.set(None);
                cur_guard.ptrs.recalc_bucket.set(None);
                cur_guard.ptrs.recalc_state.set(RecalcState::Ready);
                return true;
            }

            found_prev = Some(cur_ptr);
            head_ptr = unsafe { cur_ptr.lookup_unchecked() }.ptrs.next.get();
        }
    }

    false
}

unsafe fn free(ptr: NodePtr) {
    let guard = unsafe { ptr.lookup_unchecked() };
    let graph: &Graph2 = unsafe { &*guard.ptrs.graph };

    ////////////////////////////////////////////////////////////////////////////////
    // NOTE:
    // - 这里是在 “handle_count 刚刚归零 -> 触发 free()” 的时刻。
    // - 即使后续因为借用冲突走了 enqueue_free_retry，这里仍然能看到“是谁触发了 free”。
    ////////////////////////////////////////////////////////////////////////////////
    let cfg = free_trace_cfg();
    if cfg.enabled {
        let token = guard.slot_token.get();
        let debug_info = guard.debug_info.get()._to_string();
        if cfg.should_log(token, debug_info.as_str()) {
            eprintln!("[anchors][free] token={token} debug={debug_info}");
            if cfg.backtrace {
                let bt = Backtrace::force_capture();
                eprintln!("[anchors][free] backtrace:\n{bt}");
            }
        }
    }

    // epoch 内禁止 token 物理回收/复用：只允许 retire，等待 epoch end 再统一 reclaim。
    if graph.epoch_depth.get() > 0 {
        graph.retire_free(ptr);
        return;
    }

    debug_assert_eq!(
        graph.epoch_depth.get(),
        0,
        "slotmap: free 不应在 epoch 内执行 remove"
    );
    if unsafe { !graph.nodes.remove(ptr) } {
        graph.enqueue_free_retry(ptr);
    }
}

pub fn height(node: NodeGuard<'_>) -> usize {
    node.ptrs.height.get()
}

pub fn needs_recalc(node: NodeGuard<'_>) {
    if node.ptrs.recalc_state.get() != RecalcState::Ready {
        // already in recalc queue, or already pending recalc
        return;
    }
    node.ptrs.recalc_state.set(RecalcState::Needed);
}

pub fn recalc_state(node: NodeGuard<'_>) -> RecalcState {
    node.ptrs.recalc_state.get()
}

#[cfg(test)]
mod test {
    use super::*;

    fn to_vec<I: IntoIterator>(iter: I) -> Vec<I::Item> {
        iter.into_iter().collect()
    }

    #[test]
    fn set_edge_updates_correctly() {
        let graph = Graph2::new(256);
        graph.with(|guard| {
            let a = guard.insert_testing_guard();
            let b = guard.insert_testing_guard();
            let empty: Vec<NodeGuard<'_>> = vec![];

            assert_eq!(empty, to_vec(a.necessary_children()));
            assert_eq!(empty, to_vec(a.clean_parents()));
            assert_eq!(empty, to_vec(b.necessary_children()));
            assert_eq!(empty, to_vec(b.clean_parents()));
            assert!(a.necessary_count.get() == 0);
            assert!(b.necessary_count.get() == 0);

            assert_eq!(Ok(false), ensure_height_increases(a, b));
            assert_eq!(Ok(true), ensure_height_increases(a, b));
            a.add_clean_parent(b);

            assert_eq!(empty, to_vec(a.necessary_children()));
            assert_eq!(vec![b], to_vec(a.clean_parents()));
            assert_eq!(empty, to_vec(b.necessary_children()));
            assert_eq!(empty, to_vec(b.clean_parents()));
            assert!(a.necessary_count.get() == 0);
            assert!(b.necessary_count.get() == 0);

            assert_eq!(Ok(true), ensure_height_increases(a, b));
            b.add_necessary_child(a);

            assert_eq!(empty, to_vec(a.necessary_children()));
            assert_eq!(vec![b], to_vec(a.clean_parents()));
            assert_eq!(vec![a], to_vec(b.necessary_children()));
            assert_eq!(empty, to_vec(b.clean_parents()));
            assert!(a.necessary_count.get() > 0);
            assert!(b.necessary_count.get() == 0);

            let _ = a.drain_clean_parents();

            assert_eq!(empty, to_vec(a.necessary_children()));
            assert_eq!(empty, to_vec(a.clean_parents()));
            assert_eq!(vec![a], to_vec(b.necessary_children()));
            assert_eq!(empty, to_vec(b.clean_parents()));
            assert!(a.necessary_count.get() > 0);
            assert!(b.necessary_count.get() == 0);

            let _ = b.drain_necessary_children();

            assert_eq!(empty, to_vec(a.necessary_children()));
            assert_eq!(empty, to_vec(a.clean_parents()));
            assert_eq!(empty, to_vec(b.necessary_children()));
            assert_eq!(empty, to_vec(b.clean_parents()));
            assert!(a.necessary_count.get() == 0);
            assert!(b.necessary_count.get() == 0);
        });
    }

    #[test]
    fn height_calculated_correctly() {
        let graph = Graph2::new(256);
        graph.with(|guard| {
            let a = guard.insert_testing_guard();
            let b = guard.insert_testing_guard();
            let c = guard.insert_testing_guard();

            assert_eq!(0, height(a));
            assert_eq!(0, height(b));
            assert_eq!(0, height(c));

            assert_eq!(Ok(false), ensure_height_increases(b, c));
            assert_eq!(Ok(true), ensure_height_increases(b, c));
            b.add_clean_parent(c);

            assert_eq!(0, height(a));
            assert_eq!(0, height(b));
            assert_eq!(1, height(c));

            assert_eq!(Ok(false), ensure_height_increases(a, b));
            assert_eq!(Ok(true), ensure_height_increases(a, b));
            a.add_clean_parent(b);

            assert_eq!(0, height(a));
            assert_eq!(1, height(b));
            assert_eq!(2, height(c));

            let _ = a.drain_clean_parents();

            assert_eq!(0, height(a));
            assert_eq!(1, height(b));
            assert_eq!(2, height(c));
        })
    }

    #[test]
    fn cycles_cause_error() {
        let graph = Graph2::new(256);
        graph.with(|guard| {
            let b = guard.insert_testing_guard();
            let c = guard.insert_testing_guard();
            ensure_height_increases(b, c).unwrap();
            b.add_clean_parent(c);
            ensure_height_increases(c, b).unwrap_err();
        })
    }

    #[test]
    fn non_cycles_wont_cause_errors() {
        let graph = Graph2::new(256);
        graph.with(|guard| {
            let a = guard.insert_testing_guard();
            let b = guard.insert_testing_guard();
            let c = guard.insert_testing_guard();
            let d = guard.insert_testing_guard();
            let e = guard.insert_testing_guard();

            ensure_height_increases(b, c).unwrap();
            b.add_clean_parent(c);
            ensure_height_increases(c, e).unwrap();
            c.add_clean_parent(e);
            ensure_height_increases(b, d).unwrap();
            b.add_clean_parent(d);
            ensure_height_increases(d, e).unwrap();
            d.add_clean_parent(e);
            ensure_height_increases(a, b).unwrap();
            a.add_clean_parent(b);
        })
    }

    #[test]
    fn test_insert_pop() {
        let graph = Graph2::new(10);
        graph.with(|guard| {
            let a = guard.insert_testing_guard();
            set_min_height(a, 0).unwrap();
            let b = guard.insert_testing_guard();
            set_min_height(b, 5).unwrap();
            let c = guard.insert_testing_guard();
            set_min_height(c, 3).unwrap();
            let d = guard.insert_testing_guard();
            set_min_height(d, 4).unwrap();
            let e = guard.insert_testing_guard();
            set_min_height(e, 1).unwrap();
            let e2 = guard.insert_testing_guard();
            set_min_height(e2, 1).unwrap();
            let e3 = guard.insert_testing_guard();
            set_min_height(e3, 1).unwrap();

            guard.queue_recalc(a);
            guard.queue_recalc(a);
            guard.queue_recalc(a);
            guard.queue_recalc(b);
            guard.queue_recalc(c);
            guard.queue_recalc(d);

            assert_eq!(Some(a), guard.recalc_pop_next().map(|(_, v)| v));
            assert_eq!(Some(c), guard.recalc_pop_next().map(|(_, v)| v));
            assert_eq!(Some(d), guard.recalc_pop_next().map(|(_, v)| v));

            guard.queue_recalc(e);
            guard.queue_recalc(e2);
            guard.queue_recalc(e3);

            assert_eq!(Some(e3), guard.recalc_pop_next().map(|(_, v)| v));
            assert_eq!(Some(e2), guard.recalc_pop_next().map(|(_, v)| v));
            assert_eq!(Some(e), guard.recalc_pop_next().map(|(_, v)| v));
            assert_eq!(Some(b), guard.recalc_pop_next().map(|(_, v)| v));

            assert_eq!(None, guard.recalc_pop_next().map(|(_, v)| v));
        })
    }

    #[test]
    fn stale_queue_entries_are_skipped_after_free() {
        let graph = Graph2::new(10);
        graph.with(|guard| {
            let node = guard.insert_testing_guard();
            set_min_height(node, 1).unwrap();
            guard.queue_recalc(node);

            // ── 模拟：节点在入队后被回收（anchor 清空），但队列仍残留旧指针。
            unsafe {
                *node.anchor.get() = None;
            }
            node.ptrs.handle_count.set(0);

            // 修复前这里会返回 Some(node)，后续重算 panic；修复后应直接跳过。
            assert!(
                guard.recalc_pop_next().is_none(),
                "已移除的 anchor 不应再次出队重算"
            );
        });
    }

    #[test]
    fn queue_recalc_auto_expands_capacity() {
        let graph = Graph2::new(4);
        graph.with(|guard| {
            let node = guard.insert_testing_guard();
            // 人工设置较大的高度，模拟深链依赖场景。
            node.ptrs.height.set(12);
            guard.queue_recalc(node);
        });
        assert!(graph.recalc_queues.borrow().len() >= 13);
    }

    #[test]
    fn queue_recalc_prefers_ratio_growth() {
        let graph = Graph2::new(10);
        graph.with(|guard| {
            let node = guard.insert_testing_guard();
            // ── 高度刚好落在旧容量边界，验证扩容会按 1.2 倍执行而非只补足 1 个槽位。
            node.ptrs.height.set(10);
            guard.queue_recalc(node);
        });
        assert_eq!(12, graph.recalc_queues.borrow().len());
    }

    #[test]
    fn insert_above_initial_height_does_not_panic() {
        let graph = Graph2::new(10);
        graph.with(|guard| {
            let a = guard.insert_testing_guard();
            set_min_height(a, 10).unwrap();
            guard.queue_recalc(a);
        });
        assert!(graph.recalc_queues.borrow().len() >= 11);
    }

    #[test]
    fn test_free_list() {
        use crate::expert::AnchorHandle;
        let graph = Graph2::new(10);
        let a = graph.insert_testing();
        let b = graph.insert_testing();
        let c = graph.insert_testing();

        let a_token = a.token();
        let b_token = b.token();
        let c_token = c.token();

        std::mem::drop(a);
        std::mem::drop(b);
        std::mem::drop(c);

        let c = graph.insert_testing();
        let b = graph.insert_testing();
        let a = graph.insert_testing();
        let d = graph.insert_testing();

        assert!(a.token().token > a_token.token);
        assert!(b.token().token > b_token.token);
        assert!(c.token().token > c_token.token);
        let d_token = d.token();

        std::mem::drop(c);
        std::mem::drop(a);
        std::mem::drop(b);
        std::mem::drop(d);

        let d = graph.insert_testing();
        let b = graph.insert_testing();
        let a = graph.insert_testing();
        let c = graph.insert_testing();

        assert!(a.token().token > a_token.token);
        assert!(b.token().token > b_token.token);
        assert!(c.token().token > c_token.token);
        assert!(d.token().token > d_token.token);
    }

    #[test]
    fn drop_parent_detaches_from_child_clean_parents() {
        ////////////////////////////////////////////////////////////////////////////////
        // 回归测试：避免 “free 后槽位复用 -> child 仍持有旧 parent NodePtr -> dirty 误投递”。
        //
        // 关键断言：
        // - parent drop（触发 free/remove）后，child.clean_parents 不应再包含该 parent 指针。
        ////////////////////////////////////////////////////////////////////////////////
        use crate::expert::AnchorHandle;

        let graph = Graph2::new(10);
        let child_handle = graph.insert_testing();
        let parent_handle = graph.insert_testing();

        let child_token = child_handle.token();
        let parent_token = parent_handle.token();

        graph.with(|guard| {
            let child = guard.get(child_token).expect("child 应当存在");
            let parent = guard.get(parent_token).expect("parent 应当存在");

            // 手工构造一条边：parent 依赖 child
            // - child 记录 parent（用于 dirty 时通知依赖者）
            // - parent 记录 child（用于生命周期/必要性跟踪）
            child.add_clean_parent(parent);
            parent.add_necessary_child(child);

            assert_eq!(vec![parent], to_vec(child.clean_parents()));
        });

        // drop parent 触发 free/remove；修复前：child.clean_parents 会残留旧 parent 指针。
        drop(parent_handle);

        graph.with(|guard| {
            let child = guard.get(child_token).expect("child 应当仍存在");
            let empty: Vec<NodeGuard<'_>> = vec![];
            assert_eq!(empty, to_vec(child.clean_parents()));
        });
    }
}
