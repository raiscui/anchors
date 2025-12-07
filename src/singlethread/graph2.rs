use super::{AnchorDebugInfo, Engine, EngineContext, Generation, GenericAnchor};
use std::cell::{Cell, RefCell, UnsafeCell};
use std::rc::Rc;

#[cfg(feature = "anchors_slotmap")]
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
            debug_assert_eq!(ptr.graph, self.graph);
            // unsafe: graph 指针来源受 NodeStoreGuard 生命周期约束，确保在 SlotMap 内有效。
            let slots = unsafe { &*(*self.graph).slots.get() };
            slots
                .get(ptr.key)
                .expect("dangling NodePtr: 已释放或跨 Graph2 使用")
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
            // unsafe: NodeGuard 持有 graph 生命周期，保证 slots 指针有效。
            let slots = unsafe { &*(*self.graph).slots.get() };
            slots
                .get(self.key)
                .expect("dangling NodeGuard: 节点已被移除")
        }

        pub unsafe fn lookup_ptr(&self, ptr: NodePtr<T>) -> &T {
            debug_assert_eq!(ptr.graph, self.graph);
            // unsafe: graph 指针来自同一 NodeStore，确保访问合法。
            let slots = unsafe { &*(*self.graph).slots.get() };
            slots.get(ptr.key).expect("dangling NodePtr: 节点已被移除")
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

    #[cfg(feature = "anchors_slotmap")]
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
            graph.inc_free_attempt();
            if guard.anchor_locked.get() {
                graph.inc_gc_skipped();
                return false;
            }

            if unsafe { (&*guard.anchor.get()).is_none() } {
                graph.inc_free_skip();
                return true;
            }

            // 尝试非阻塞清理必要子节点，若外部仍借用则跳过以避免 RefCell panic。
            let drained_children =
                if let Ok(mut children) = guard.ptrs.necessary_children.try_borrow_mut() {
                    for child in children.iter() {
                        let count = &unsafe { guard.0.lookup_ptr(*child) }.necessary_count;
                        count.set(count.get().saturating_sub(1));
                    }
                    children.clear();
                    true
                } else {
                    false
                };
            if !drained_children {
                graph.inc_gc_skipped();
                return false;
            }

            guard.ptrs.clean_parent0.set(None);
            // 父列表同理，避免在外层持有不可预期借用时触发崩溃。
            let drained_parents = if let Ok(mut parents) = guard.ptrs.clean_parents.try_borrow_mut()
            {
                parents.clear();
                true
            } else {
                false
            };
            if !drained_parents {
                graph.inc_gc_skipped();
                return false;
            }

            super::dequeue_calc(graph, guard);

            let anchor_slot = unsafe { &mut *guard.anchor.get() };
            if anchor_slot.is_none() {
                return true;
            }
            let deleted_token = guard.slot_token.get();
            *anchor_slot = None;

            guard.ptrs.handle_count.set(0);
            guard.observed.set(false);
            guard.visited.set(false);
            guard.necessary_count.set(0);
            guard.ptrs.recalc_state.set(super::RecalcState::Needed);
            guard.recalc_retry.set(0);
            guard.pending_dirty.borrow_mut().clear();
            guard.pending_recalc.set(false);
            guard.anchor_locked.set(false);

            let free_head = &graph.free_head;
            if let Some(old_free) = free_head.get() {
                let old_guard = unsafe { old_free.lookup_unchecked() };
                old_guard.ptrs.prev.set(Some(ptr));
            }
            guard.ptrs.next.set(free_head.get());
            guard.ptrs.prev.set(None);
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

#[cfg(feature = "anchors_slotmap")]
use node_store as ag;

#[cfg(not(feature = "anchors_slotmap"))]
use arena_graph::raw as ag;

use std::fmt;
use std::iter::Iterator;

#[derive(PartialEq, Clone, Copy)]
pub struct NodeGuard<'gg>(ag::NodeGuard<'gg, Node>);

/// slotmap GC 计数快照，用于日志与观测。
#[cfg(feature = "anchors_slotmap")]
#[derive(Debug, Clone, Copy, Default)]
pub struct GcStatsSnapshot {
    pub gc_skipped: u64,
    pub free_skip: u64,
    pub free_attempts: u64,
    pub free_succeeded: u64,
    pub pending_free: usize,
}

#[cfg(feature = "anchors_slotmap")]
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
            .field("ptr", &unsafe { self.0.make_ptr() })
            .field("slot_token", &self.slot_token.get())
            .finish()
    }
}

#[cfg(feature = "anchors_slotmap")]
type AgGuard<'gg> = ag::NodeStoreGuard<'gg, Node>;
#[cfg(not(feature = "anchors_slotmap"))]
type AgGuard<'gg> = ag::GraphGuard<'gg, Node>;

#[derive(Default, Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum RecalcState {
    #[default]
    Needed,
    Pending,
    Ready,
}

thread_local! {
    pub static NEXT_TOKEN: Cell<u32> = Cell::new(0);
}

pub struct Graph2 {
    #[cfg(feature = "anchors_slotmap")]
    nodes: ag::NodeStore<Node>,
    #[cfg(not(feature = "anchors_slotmap"))]
    nodes: ag::Graph<Node>,
    #[cfg(feature = "anchors_slotmap")]
    active_nodes: Cell<usize>,
    #[cfg(feature = "anchors_slotmap")]
    pending_free: RefCell<Vec<NodePtr>>,
    #[cfg(feature = "anchors_slotmap")]
    gc_skipped: Cell<u64>,
    #[cfg(feature = "anchors_slotmap")]
    free_skip: Cell<u64>,
    #[cfg(feature = "anchors_slotmap")]
    free_attempts: Cell<u64>,
    #[cfg(feature = "anchors_slotmap")]
    free_succeeded: Cell<u64>,
    graph_token: u32,
    #[cfg(feature = "anchors_slotmap")]
    token_counter: Cell<u64>,
    #[cfg(feature = "anchors_slotmap")]
    last_deleted_token: Cell<Option<u64>>,

    still_alive: Rc<Cell<bool>>,

    /// height -> first node in that height's queue
    recalc_queues: RefCell<Vec<Option<NodePtr>>>,
    recalc_min_height: Cell<usize>,
    recalc_max_height: Cell<usize>,

    /// pointer to head of linked list of free nodes
    free_head: Box<Cell<Option<NodePtr>>>,
}

#[derive(Clone, Copy)]
pub struct Graph2Guard<'gg> {
    nodes: AgGuard<'gg>,
    graph: &'gg Graph2,
}

pub struct Node {
    pub observed: Cell<bool>,

    /// bool used during height incrementing to check for loops
    pub visited: Cell<bool>,

    /// number of nodes that list this node as a necessary child
    pub necessary_count: Cell<usize>,

    pub slot_token: Cell<u64>,

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
    pub anchor: UnsafeCell<Option<Box<dyn GenericAnchor>>>,
    /// 标记当前 anchor 是否正被 poll，避免重入。
    pub anchor_locked: Cell<bool>,

    pub ptrs: NodePtrs,
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

pub struct NodePtrs {
    /// first parent, remaining parents. unsorted, duplicates may exist
    clean_parent0: Cell<Option<NodePtr>>,
    clean_parents: RefCell<Vec<NodePtr>>,

    graph: *const Graph2,

    /// Next node in either recalc linked list for this height, or if node is in the free list, the free linked list.
    /// If this is the last node, None.
    next: Cell<Option<NodePtr>>,
    /// Prev node in either recalc linked list for this height, or if node is in the free list, the free linked list.
    /// If this is the head node, None.
    prev: Cell<Option<NodePtr>>,
    recalc_state: Cell<RecalcState>,

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
    pub fn key(self) -> NodeKey {
        NodeKey {
            ptr: unsafe { self.0.make_ptr() },
            token: self.slot_token.get(),
        }
    }

    pub fn add_clean_parent(self, parent: NodeGuard<'a>) {
        if self.ptrs.clean_parent0.get().is_none() {
            self.ptrs
                .clean_parent0
                .set(Some(unsafe { parent.0.make_ptr() }))
        } else {
            self.ptrs
                .clean_parents
                .borrow_mut()
                .push(unsafe { parent.0.make_ptr() })
        }
        if std::env::var("ANCHORS_DEBUG_PARENT_LINK")
            .map(|v| v != "0")
            .unwrap_or(false)
        {
            // 调试输出：带上节点的 debug_info，便于定位父子是谁
            let child_dbg = self.debug_info.get()._to_string();
            let parent_dbg = parent.debug_info.get()._to_string();
            println!(
                "add_clean_parent child={:?} ({}) parent={:?} ({})",
                self.key().raw_token(),
                child_dbg,
                parent.key().raw_token(),
                parent_dbg
            );
        }
    }

    pub fn clean_parents(&self) -> Vec<NodeGuard<'a>> {
        let mut out = Vec::new();
        if let Some(first) = self.ptrs.clean_parent0.get() {
            out.push(NodeGuard(unsafe { first.lookup_unchecked() }));
        }
        let parents = self.ptrs.clean_parents.borrow();
        out.extend(
            parents
                .iter()
                .map(|ptr| NodeGuard(unsafe { ptr.lookup_unchecked() })),
        );
        if std::env::var("ANCHORS_DEBUG_PARENT_FLOW")
            .map(|v| v != "0")
            .unwrap_or(false)
        {
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
        if let Some(first) = self.ptrs.clean_parent0.get() {
            f(NodeGuard(unsafe { first.lookup_unchecked() }));
        }
        for ptr in self.ptrs.clean_parents.borrow().iter().copied() {
            f(NodeGuard(unsafe { ptr.lookup_unchecked() }));
        }
    }

    /// 取出并清空父节点列表，供 mark_dirty 等热路径复用。
    pub fn drain_parents(&self, mut f: impl FnMut(NodeGuard<'a>)) {
        if let Some(first) = self.ptrs.clean_parent0.get() {
            self.ptrs.clean_parent0.set(None);
            f(NodeGuard(unsafe { first.lookup_unchecked() }));
        }
        let mut parents = self.ptrs.clean_parents.borrow_mut();
        for ptr in parents.drain(..) {
            f(NodeGuard(unsafe { ptr.lookup_unchecked() }));
        }
    }

    /// 父节点数量，用于日志统计，避免 Vec 分配。
    pub fn parents_len(&self) -> usize {
        let extra = self.ptrs.clean_parents.borrow().len();
        extra + usize::from(self.ptrs.clean_parent0.get().is_some())
    }

    pub fn drain_clean_parents(&self) -> Vec<NodeGuard<'a>> {
        let res = self.clean_parents();
        self.ptrs.clean_parent0.set(None);
        self.ptrs.clean_parents.borrow_mut().clear();
        if std::env::var("ANCHORS_DEBUG_PARENT_FLOW")
            .map(|v| v != "0")
            .unwrap_or(false)
        {
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
        let child_ptr = unsafe { child.0.make_ptr() };
        if let Err(i) = necessary_children.binary_search(&child_ptr) {
            necessary_children.insert(i, child_ptr);
            child.necessary_count.set(child.necessary_count.get() + 1)
        }
    }

    pub fn remove_necessary_child(&self, child: NodeGuard<'a>) {
        let mut necessary_children = self.ptrs.necessary_children.borrow_mut();
        let child_ptr = unsafe { child.0.make_ptr() };
        if let Ok(i) = necessary_children.binary_search(&child_ptr) {
            necessary_children.remove(i);
            child.necessary_count.set(child.necessary_count.get() - 1)
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
        let mut children = self.ptrs.necessary_children.borrow_mut();
        for child in children.iter() {
            let count = &unsafe { self.0.lookup_ptr(*child) }.necessary_count;
            count.set(count.get().saturating_sub(1));
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
    #[cfg(feature = "anchors_slotmap")]
    pub fn active_nodes(&self) -> usize {
        self.graph.active_nodes.get()
    }

    #[cfg(feature = "anchors_slotmap")]
    pub fn gc_stats(&self) -> GcStatsSnapshot {
        self.graph.gc_stats_snapshot()
    }

    #[cfg(feature = "anchors_slotmap")]
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

    #[cfg(feature = "anchors_slotmap")]
    #[inline]
    pub fn has_pending_free(&self) -> bool {
        !self.graph.pending_free.borrow().is_empty()
    }

    #[cfg(feature = "anchors_slotmap")]
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
                node.ptrs.recalc_state.set(RecalcState::Ready);
                // ┌─────────────────────────────────────────────────────────────┐
                // │ 队列可能残留“已回收节点”（anchor 已置空、handle_count 归零）。│
                // │ 这些节点若继续出队将触发重算 panic，故在此直接跳过。           │
                // └─────────────────────────────────────────────────────────────┘
                if unsafe { (*node.anchor.get()).is_none() } {
                    if std::env::var("ANCHORS_DEBUG_QUEUE")
                        .map(|v| v != "0")
                        .unwrap_or(false)
                    {
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
                if std::env::var("ANCHORS_DEBUG_QUEUE")
                    .map(|v| v != "0")
                    .unwrap_or(false)
                {
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
        self.graph.recalc_max_height.set(0);
        None
    }

    pub fn queue_recalc(&self, node: NodeGuard<'gg>) {
        if node.ptrs.recalc_state.get() == RecalcState::Pending {
            if std::env::var("ANCHORS_DEBUG_QUEUE")
                .map(|v| v != "0")
                .unwrap_or(false)
            {
                println!(
                    "queue_recalc skip (already pending) token={:?} debug={}",
                    node.key().raw_token(),
                    node.debug_info.get()._to_string()
                );
            }
            // already in recalc queue
            return;
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
        if std::env::var("ANCHORS_DEBUG_QUEUE")
            .map(|v| v != "0")
            .unwrap_or(false)
        {
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
                .set(Some(unsafe { node.0.make_ptr() }));
            node.ptrs.next.set(Some(old));
        } else {
            if self.graph.recalc_min_height.get() > node_height {
                self.graph.recalc_min_height.set(node_height);
            }
            if self.graph.recalc_max_height.get() < node_height {
                self.graph.recalc_max_height.set(node_height);
            }
        }
        recalc_queues[node_height] = Some(unsafe { node.0.make_ptr() });
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

    /// 读取已 Ready 节点的输出副本，不触发额外 request，仅限 anchors_slotmap。
    #[cfg(feature = "anchors_slotmap")]
    pub fn output_cached<O: Clone + 'static>(&self, engine: &Engine, node: NodeGuard<'gg>) -> O {
        let anchor_impl = unsafe {
            (*node.anchor.get())
                .as_ref()
                .expect("slotmap: anchor 缺失，无法读取缓存输出")
        };
        anchor_impl
            .output(&mut EngineContext { engine })
            .downcast_ref::<O>()
            .expect("slotmap: output_cached 类型不匹配")
            .clone()
    }
}

impl Graph2 {
    #[cfg(feature = "anchors_slotmap")]
    fn next_slot_token(&self) -> u64 {
        let current = self.token_counter.get();
        self.token_counter.set(current + 1);
        current
    }

    #[cfg(feature = "anchors_slotmap")]
    pub fn token_counter(&self) -> u64 {
        self.token_counter.get()
    }

    #[cfg(feature = "anchors_slotmap")]
    pub fn last_deleted_token(&self) -> Option<u64> {
        self.last_deleted_token.get()
    }

    #[cfg(feature = "anchors_slotmap")]
    #[inline]
    fn inc_gc_skipped(&self) {
        self.gc_skipped.set(self.gc_skipped.get().saturating_add(1));
    }

    #[cfg(feature = "anchors_slotmap")]
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

    #[cfg(feature = "anchors_slotmap")]
    pub fn gc_stats_snapshot(&self) -> GcStatsSnapshot {
        GcStatsSnapshot {
            gc_skipped: self.gc_skipped.get(),
            free_skip: self.free_skip.get(),
            free_attempts: self.free_attempts.get(),
            free_succeeded: self.free_succeeded.get(),
            pending_free: self.pending_free.borrow().len(),
        }
    }

    #[cfg(feature = "anchors_slotmap")]
    fn enqueue_free_retry(&self, ptr: NodePtr) {
        let mut pending = self.pending_free.borrow_mut();
        if !pending.iter().any(|p| *p == ptr) {
            pending.push(ptr);
        }
    }

    #[cfg(feature = "anchors_slotmap")]
    fn retry_pending_free(&self) {
        let mut pending = self.pending_free.borrow_mut();
        if pending.is_empty() {
            return;
        }

        let mut still_pending = Vec::new();
        for ptr in pending.drain(..) {
            let freed = unsafe { self.nodes.remove(ptr) };
            if !freed {
                if !still_pending.iter().any(|p| *p == ptr) {
                    still_pending.push(ptr);
                }
            }
        }
        *pending = still_pending;
    }

    #[cfg(not(feature = "anchors_slotmap"))]
    fn next_slot_token(&self) -> u64 {
        self.graph_token as u64
    }

    pub fn new(max_height: usize) -> Self {
        Self {
            #[cfg(feature = "anchors_slotmap")]
            nodes: ag::NodeStore::new(),
            #[cfg(not(feature = "anchors_slotmap"))]
            nodes: ag::Graph::new(),
            graph_token: NEXT_TOKEN.with(|token| {
                let n = token.get();
                token.set(n + 1);
                n
            }),
            #[cfg(feature = "anchors_slotmap")]
            token_counter: Cell::new(0),
            #[cfg(feature = "anchors_slotmap")]
            active_nodes: Cell::new(0),
            #[cfg(feature = "anchors_slotmap")]
            pending_free: RefCell::new(vec![]),
            #[cfg(feature = "anchors_slotmap")]
            gc_skipped: Cell::new(0),
            #[cfg(feature = "anchors_slotmap")]
            free_skip: Cell::new(0),
            #[cfg(feature = "anchors_slotmap")]
            free_attempts: Cell::new(0),
            #[cfg(feature = "anchors_slotmap")]
            free_succeeded: Cell::new(0),
            #[cfg(feature = "anchors_slotmap")]
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
        func(Graph2Guard { nodes, graph: self })
    }

    #[cfg(test)]
    pub fn insert_testing(&self) -> AnchorHandle {
        self.insert(
            Box::new(crate::expert::constant::Constant::new_raw_testing(123)),
            AnchorDebugInfo {
                location: None,
                type_info: "testing dummy anchor",
            },
        )
    }

    pub(super) fn insert(
        &'_ self,
        anchor: Box<dyn GenericAnchor>,
        debug_info: AnchorDebugInfo,
    ) -> AnchorHandle {
        self.nodes.with(|nodes| {
            let guard = if let Some(free_head) = self.free_head.get() {
                let guard = NodeGuard(unsafe { free_head.lookup_unchecked() });
                self.free_head.set(guard.ptrs.next.get());
                if let Some(next_ptr) = guard.ptrs.next.get() {
                    let next_node = unsafe { nodes.lookup_ptr(next_ptr) };
                    next_node.ptrs.prev.set(None);
                }
                guard.observed.set(false);
                guard.visited.set(false);
                guard.necessary_count.set(0);
                guard.slot_token.set(self.next_slot_token());
                guard.ptrs.clean_parent0.set(None);
                guard.ptrs.clean_parents.replace(vec![]);
                guard.ptrs.recalc_state.set(RecalcState::Needed);
                guard.ptrs.necessary_children.replace(vec![]);
                guard.ptrs.height.set(0);
                guard.ptrs.handle_count.set(1);
                guard.ptrs.prev.set(None);
                guard.ptrs.next.set(None);
                guard.debug_info.set(debug_info);
                guard.last_ready.set(None);
                guard.last_update.set(None);
                guard.recalc_retry.set(0);
                guard.pending_dirty.replace(vec![]);
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
                    ptrs: NodePtrs {
                        clean_parent0: Cell::new(None),
                        clean_parents: RefCell::new(vec![]),
                        graph: self,
                        next: Cell::new(None),
                        prev: Cell::new(None),
                        recalc_state: Cell::new(RecalcState::Needed),
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
            #[cfg(feature = "anchors_slotmap")]
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
    // 剪枝：高度已满足，无需递归向上回填。
    if height(node) >= min_height {
        return Ok(());
    }
    if node.visited.get() {
        return Err(());
    }
    // 进入递归时先打上 visited 标记，退出前无论成功/失败都要清理，避免后续调用误判成环。
    node.visited.set(true);
    let res = (|| {
        if height(node) < min_height {
            node.ptrs.height.set(min_height);
            let mut did_err = false;
            // 迭代式堆栈，避免深链递归爆栈与重复 visited 标记。
            let mut stack = Vec::with_capacity(4);
            node.for_each_parent(|p| stack.push(p));
            while let Some(parent) = stack.pop() {
                if let Err(_loop_ids) = set_min_height(parent, min_height + 1) {
                    did_err = true;
                }
            }
            if did_err {
                return Err(());
            }
        }
        Ok(())
    })();
    node.visited.set(false);
    res
}

/// 辅助：当节点已经 Pending 但需要“重新排队”时，先摘下旧位置再重新入队，避免 recalc_min_height 已越过导致饥饿。
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
pub fn set_recalc_ready(node: NodeGuard<'_>) {
    node.ptrs.recalc_state.set(RecalcState::Ready);
}

fn dequeue_calc(graph: &Graph2, node: NodeGuard<'_>) {
    if node.ptrs.recalc_state.get() != RecalcState::Pending {
        return;
    }
    // ─────────────────────────────────────────────────────────────
    // 说明：高度可能在 Pending 队列期间被提升，prev/next 指针也可能失效。
    // 这里通过遍历队列重新定位节点，防御性摘除，避免断言直接崩溃。
    let height_idx = node.ptrs.height.get();
    let mut recalc_queues = graph.recalc_queues.borrow_mut();
    let mut head_ptr = recalc_queues.get(height_idx).copied().flatten();

    let target_ptr = unsafe { node.0.make_ptr() };
    let mut found_prev: Option<NodePtr> = None;
    let mut found_cur: Option<NodePtr> = None;

    while let Some(cur_ptr) = head_ptr {
        if cur_ptr == target_ptr {
            found_cur = Some(cur_ptr);
            break;
        }
        found_prev = head_ptr;
        head_ptr = unsafe { cur_ptr.lookup_unchecked() }.ptrs.next.get();
    }

    // 若队列中已找不到该节点，直接重置状态后返回，避免 panic 连环崩溃。
    let Some(cur_ptr) = found_cur else {
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
        return;
    };

    let cur_guard = unsafe { cur_ptr.lookup_unchecked() };
    let next_ptr = cur_guard.ptrs.next.get();

    if let Some(prev_ptr) = found_prev {
        unsafe { prev_ptr.lookup_unchecked() }
            .ptrs
            .next
            .set(next_ptr);
    } else {
        recalc_queues[height_idx] = next_ptr;
    }

    if let Some(next) = next_ptr {
        unsafe { next.lookup_unchecked() }.ptrs.prev.set(found_prev);
    }

    cur_guard.ptrs.prev.set(None);
    cur_guard.ptrs.next.set(None);
    cur_guard.ptrs.recalc_state.set(RecalcState::Ready);
}

#[cfg(feature = "anchors_slotmap")]
unsafe fn free(ptr: NodePtr) {
    let graph: &Graph2 = {
        let guard = unsafe { ptr.lookup_unchecked() };
        // unsafe: graph 指针来源于当前 slotmap 节点，生命周期受 NodeStore 约束。
        unsafe { &*guard.ptrs.graph }
    };

    if unsafe { !graph.nodes.remove(ptr) } {
        graph.enqueue_free_retry(ptr);
    }
}

#[cfg(not(feature = "anchors_slotmap"))]
unsafe fn free(ptr: NodePtr) {
    let guard = NodeGuard(unsafe { ptr.lookup_unchecked() });
    let anchor_slot = unsafe { &mut *guard.anchor.get() };
    if anchor_slot.is_none() {
        return;
    }
    let _ = guard.drain_necessary_children();
    let _ = guard.drain_clean_parents();
    // unsafe: graph 指针来自节点内部，保持同生命周期。
    let graph = unsafe { &*guard.ptrs.graph };
    dequeue_calc(graph, guard);
    let free_head = &graph.free_head;
    let old_free = free_head.get();
    if let Some(old_free) = old_free {
        unsafe {
            guard.0.lookup_ptr(old_free).ptrs.prev.set(Some(ptr));
        }
    }
    guard.ptrs.next.set(old_free);
    free_head.set(Some(ptr));
    *anchor_slot = None;
    guard.anchor_locked.set(false);
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

        #[cfg(feature = "anchors_slotmap")]
        {
            assert!(a.token().token > a_token.token);
            assert!(b.token().token > b_token.token);
            assert!(c.token().token > c_token.token);
        }
        #[cfg(not(feature = "anchors_slotmap"))]
        {
            assert_eq!(a_token, a.token());
            assert_eq!(b_token, b.token());
            assert_eq!(c_token, c.token());
        }
        let d_token = d.token();

        std::mem::drop(c);
        std::mem::drop(a);
        std::mem::drop(b);
        std::mem::drop(d);

        let d = graph.insert_testing();
        let b = graph.insert_testing();
        let a = graph.insert_testing();
        let c = graph.insert_testing();

        #[cfg(feature = "anchors_slotmap")]
        {
            assert!(a.token().token > a_token.token);
            assert!(b.token().token > b_token.token);
            assert!(c.token().token > c_token.token);
            assert!(d.token().token > d_token.token);
        }
        #[cfg(not(feature = "anchors_slotmap"))]
        {
            assert_eq!(a_token, a.token());
            assert_eq!(b_token, b.token());
            assert_eq!(c_token, c.token());
            assert_eq!(d_token, d.token());
        }
    }
}
