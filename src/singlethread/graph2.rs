use super::{AnchorDebugInfo, Generation, GenericAnchor};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

#[cfg(feature = "anchors_slotmap")]
use crate::telemetry::{SlotmapEventKind, record_slotmap_event};

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
            let slots = &*(*self.graph).slots.get();
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
            unsafe {
                let slots = &*(*self.graph).slots.get();
                slots
                    .get(self.key)
                    .expect("dangling NodeGuard: 节点已被移除")
            }
        }

        pub unsafe fn lookup_ptr(&self, ptr: NodePtr<T>) -> &T {
            debug_assert_eq!(ptr.graph, self.graph);
            let slots = &*(*self.graph).slots.get();
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
        pub unsafe fn remove(&self, ptr: NodePtr<super::Node>) {
            let raw_guard = ptr.lookup_unchecked();
            let guard = super::NodeGuard(raw_guard);
            let graph: &super::Graph2 = &*guard.ptrs.graph;
            if guard.anchor.borrow().is_none() {
                graph
                    .node_metrics
                    .record(super::SlotmapEventKind::GcSkipped);
                return;
            }

            let _ = guard.drain_necessary_children();
            let _ = guard.drain_clean_parents();
            super::dequeue_calc(graph, guard);

            let free_head = &graph.free_head;
            if let Some(old_free) = free_head.get() {
                let old_guard = old_free.lookup_unchecked();
                old_guard.ptrs.prev.set(Some(ptr));
            }
            guard.ptrs.next.set(free_head.get());
            guard.ptrs.prev.set(None);
            free_head.set(Some(ptr));

            guard.ptrs.handle_count.set(0);
            guard.observed.set(false);
            guard.visited.set(false);
            guard.necessary_count.set(0);
            guard.ptrs.recalc_state.set(super::RecalcState::Needed);
            guard.ptrs.necessary_children.borrow_mut().clear();
            guard.ptrs.clean_parent0.set(None);
            guard.ptrs.clean_parents.borrow_mut().clear();

            *guard.anchor.borrow_mut() = None;
            graph.node_metrics.record(super::SlotmapEventKind::Free);
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

type NodePtr = ag::NodePtr<Node>;

impl<'gg> fmt::Debug for NodeGuard<'gg> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NodeGuard")
            .field("ptr", &self.0.make_ptr())
            .field("slot_token", &self.slot_token.get())
            .finish()
    }
}

#[cfg(feature = "anchors_slotmap")]
type AgGuard<'gg> = ag::NodeStoreGuard<'gg, Node>;
#[cfg(not(feature = "anchors_slotmap"))]
type AgGuard<'gg> = ag::GraphGuard<'gg, Node>;

#[cfg(feature = "anchors_slotmap")]
#[derive(Default)]
struct NodeStoreMetrics {
    alloc: Cell<u64>,
    free: Cell<u64>,
    gc_skipped: Cell<u64>,
    free_skip: Cell<u64>,
}

#[cfg(feature = "anchors_slotmap")]
impl NodeStoreMetrics {
    fn record(&self, kind: SlotmapEventKind) {
        let counter = match kind {
            SlotmapEventKind::Alloc => &self.alloc,
            SlotmapEventKind::Free => &self.free,
            SlotmapEventKind::GcSkipped => &self.gc_skipped,
            SlotmapEventKind::FreeSkip => &self.free_skip,
        };
        let next = counter.get().saturating_add(1);
        counter.set(next);
        record_slotmap_event(kind, next);
    }
}

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
    graph_token: u32,
    #[cfg(feature = "anchors_slotmap")]
    node_metrics: NodeStoreMetrics,
    #[cfg(feature = "anchors_slotmap")]
    token_counter: Cell<u64>,

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

    /// Some() if this node is still active, None otherwise
    pub anchor: RefCell<Option<Box<dyn GenericAnchor>>>,

    pub ptrs: NodePtrs,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy, Hash, PartialOrd, Ord)]
pub struct NodeKey {
    pub ptr: NodePtr,
    token: u64,
}

impl !Send for NodeKey {}
impl !Sync for NodeKey {}

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
            #[cfg(feature = "anchors_slotmap")]
            {
                let graph: &Graph2 = unsafe { &*guard.ptrs.graph };
                graph.node_metrics.record(SlotmapEventKind::FreeSkip);
            }
            return;
        }
        let count = &guard.ptrs.handle_count;
        let current = count.get();
        if current == 0 {
            #[cfg(feature = "anchors_slotmap")]
            {
                let graph: &Graph2 = unsafe { &*guard.ptrs.graph };
                graph.node_metrics.record(SlotmapEventKind::FreeSkip);
            }
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
            ptr: self.0.make_ptr(),
            token: self.slot_token.get(),
        }
    }

    pub fn add_clean_parent(self, parent: NodeGuard<'a>) {
        if self.ptrs.clean_parent0.get().is_none() {
            self.ptrs.clean_parent0.set(Some(parent.0.make_ptr()))
        } else {
            self.ptrs
                .clean_parents
                .borrow_mut()
                .push(parent.0.make_ptr())
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
        out
    }

    pub fn drain_clean_parents(&self) -> Vec<NodeGuard<'a>> {
        let res = self.clean_parents();
        self.ptrs.clean_parent0.set(None);
        self.ptrs.clean_parents.borrow_mut().clear();
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
}

impl<'gg> Graph2Guard<'gg> {
    pub fn get(&self, key: NodeKey) -> Option<NodeGuard<'gg>> {
        let node = NodeGuard(unsafe { key.ptr.lookup_unchecked() });
        if key.token != node.slot_token.get() {
            return None;
        }
        if node.anchor.borrow().is_none() {
            return None;
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
            // already in recalc queue
            return;
        }
        node.ptrs.recalc_state.set(RecalcState::Pending);
        let node_height = height(node);
        let mut recalc_queues = self.graph.recalc_queues.borrow_mut();
        if node_height >= recalc_queues.len() {
            panic!(
                "too large height error {} \n debug_info:{:?}",
                &node_height,
                node.debug_info.get()
            );
        }
        if let Some(old) = recalc_queues[node_height] {
            NodeGuard(unsafe { old.lookup_unchecked() })
                .ptrs
                .prev
                .set(Some(node.0.make_ptr()));
            node.ptrs.next.set(Some(old));
        } else {
            if self.graph.recalc_min_height.get() > node_height {
                self.graph.recalc_min_height.set(node_height);
            }
            if self.graph.recalc_max_height.get() < node_height {
                self.graph.recalc_max_height.set(node_height);
            }
        }
        recalc_queues[node_height] = Some(node.0.make_ptr());
    }
}

impl Graph2 {
    #[cfg(feature = "anchors_slotmap")]
    fn next_slot_token(&self) -> u64 {
        let current = self.token_counter.get();
        self.token_counter.set(current + 1);
        current
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
            node_metrics: NodeStoreMetrics::default(),
            #[cfg(feature = "anchors_slotmap")]
            token_counter: Cell::new(0),
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
                guard.anchor.replace(Some(anchor));
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
                    anchor: RefCell::new(Some(anchor)),
                };
                NodeGuard(nodes.insert(node))
            };
            let num = guard.key();
            #[cfg(feature = "anchors_slotmap")]
            self.node_metrics.record(SlotmapEventKind::Alloc);
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
    if node.visited.get() {
        return Err(());
    }
    node.visited.set(true);
    if height(node) < min_height {
        node.ptrs.height.set(min_height);
        let mut did_err = false;
        for parent in node.clean_parents() {
            if let Err(_loop_ids) = set_min_height(parent, min_height + 1) {
                did_err = true;
            }
        }
        if did_err {
            return Err(());
        }
    }
    node.visited.set(false);
    Ok(())
}

fn dequeue_calc(graph: &Graph2, node: NodeGuard<'_>) {
    if node.ptrs.recalc_state.get() != RecalcState::Pending {
        return;
    }
    if let Some(prev) = node.ptrs.prev.get() {
        unsafe { prev.lookup_unchecked() }
            .ptrs
            .next
            .set(node.ptrs.next.get());
    } else {
        // node was first in queue, need to set queue head to next
        let mut recalc_queues = graph.recalc_queues.borrow_mut();
        let height = node.ptrs.height.get();
        let next = node.ptrs.next.get();
        assert_eq!(
            recalc_queues[height].map(|ptr| unsafe { ptr.lookup_unchecked() }),
            Some(node.0)
        );
        recalc_queues[height] = next;
    }

    if let Some(next) = node.ptrs.next.get() {
        unsafe { next.lookup_unchecked() }
            .ptrs
            .next
            .set(node.ptrs.prev.get());
    }

    node.ptrs.prev.set(None);
    node.ptrs.next.set(None);
}

#[cfg(feature = "anchors_slotmap")]
unsafe fn free(ptr: NodePtr) {
    let guard = ptr.lookup_unchecked();
    let graph: &Graph2 = &*guard.ptrs.graph;
    graph.nodes.remove(ptr);
}

#[cfg(not(feature = "anchors_slotmap"))]
unsafe fn free(ptr: NodePtr) {
    let guard = NodeGuard(ptr.lookup_unchecked());
    if guard.anchor.borrow().is_none() {
        return;
    }
    let _ = guard.drain_necessary_children();
    let _ = guard.drain_clean_parents();
    let graph = &*guard.ptrs.graph;
    dequeue_calc(graph, guard);
    let free_head = &graph.free_head;
    let old_free = free_head.get();
    if let Some(old_free) = old_free {
        guard.0.lookup_ptr(old_free).ptrs.prev.set(Some(ptr));
    }
    guard.ptrs.next.set(old_free);
    free_head.set(Some(ptr));
    *guard.anchor.borrow_mut() = None;
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

    fn to_vec<I: std::iter::Iterator>(iter: I) -> Vec<I::Item> {
        iter.collect()
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
    #[should_panic]
    fn test_insert_above_max_height() {
        let graph = Graph2::new(10);
        graph.with(|guard| {
            let a = guard.insert_testing_guard();
            set_min_height(a, 10).unwrap();
            guard.queue_recalc(a);
        })
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
