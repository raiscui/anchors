use std::{
    alloc::{Layout, alloc, dealloc, handle_alloc_error},
    cell::RefCell,
    collections::HashMap,
    ptr::NonNull,
};

// =============================================================================
// AnchorMemPool: 单线程 anchors 的“原始内存块”复用池
//
// 设计目标：
// - 把 hot path 上的 `Box::new(inner)` 堆分配，改为“原地构造 + 原地析构 + recycle 内存块”；
// - 只用 `(size, align)` 作为 key 复用 raw block（内存无类型，只要 Layout 匹配就可复用）；
// - 观测性：暴露 hit/miss/recycle/high_watermark 等指标，便于 pprof/perf 验收。
//
// 安全边界（重要）：
// - pool 只负责 raw memory 的 alloc/recycle；
// - “何时允许 drop + recycle” 由上层 Graph2 的 epoch/reclaim 边界保证。
// =============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct LayoutKey {
    size: usize,
    align: usize,
}

impl LayoutKey {
    #[inline]
    fn new(layout: Layout) -> Self {
        Self {
            size: layout.size(),
            align: layout.align(),
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct AnchorPoolStatsSnapshot {
    /// alloc() 被调用的次数（包含 hit/miss）。
    pub alloc_calls: u64,
    /// 从池中复用成功的次数。
    pub alloc_hits: u64,
    /// 走系统分配器的次数。
    pub alloc_misses: u64,
    /// recycle() 被调用的次数。
    pub recycle_calls: u64,
    /// 当前“已借出但未归还”的总字节数（近似值）。
    pub bytes_in_use: usize,
    /// 当前缓存（池内可复用）的总字节数（近似值）。
    pub bytes_cached: usize,
    /// 历史峰值：pool 持有的总字节数（in_use + cached 的最大值）。
    pub high_watermark_bytes: usize,
    /// 当前 bucket 数量（不同 LayoutKey 的数量）。
    pub bucket_count: usize,
}

struct AnchorMemPoolInner {
    buckets: HashMap<LayoutKey, Vec<NonNull<u8>>>,

    alloc_calls: u64,
    alloc_hits: u64,
    alloc_misses: u64,
    recycle_calls: u64,

    bytes_in_use: usize,
    bytes_cached: usize,
    high_watermark_bytes: usize,
}

impl Default for AnchorMemPoolInner {
    fn default() -> Self {
        Self {
            buckets: HashMap::new(),
            alloc_calls: 0,
            alloc_hits: 0,
            alloc_misses: 0,
            recycle_calls: 0,
            bytes_in_use: 0,
            bytes_cached: 0,
            high_watermark_bytes: 0,
        }
    }
}

pub(crate) struct AnchorMemPool {
    inner: RefCell<AnchorMemPoolInner>,
}

impl AnchorMemPool {
    #[inline]
    pub(crate) fn new() -> Self {
        Self {
            inner: RefCell::new(AnchorMemPoolInner::default()),
        }
    }

    /// 分配一块符合 Layout 的 raw memory。
    ///
    /// 说明：
    /// - 返回的是未初始化内存，上层必须立刻 `ptr.write(value)` 原地构造；
    /// - 若 layout.size()==0，则返回 `NonNull::dangling()`（并计入 hit）。
    #[must_use]
    pub(crate) fn alloc(&self, layout: Layout) -> NonNull<u8> {
        let key = LayoutKey::new(layout);
        let mut inner = self.inner.borrow_mut();

        inner.alloc_calls = inner.alloc_calls.saturating_add(1);

        // ZST：不需要真实分配，但仍要保证“指针对齐”满足 Layout。
        if layout.size() == 0 {
            inner.alloc_hits = inner.alloc_hits.saturating_add(1);
            // 说明：
            // - `NonNull::<u8>::dangling()` 只保证 align=1；
            // - 这里用一个“按 align 对齐的非空地址”作为虚指针，避免后续 `ptr.write` 触发未对齐 UB。
            let addr = layout.align();
            // SAFETY: align >= 1，地址非 0；ZST 不会被真实解引用。
            return unsafe { NonNull::new_unchecked(addr as *mut u8) };
        }

        if let Some(bucket) = inner.buckets.get_mut(&key) {
            if let Some(ptr) = bucket.pop() {
                inner.alloc_hits = inner.alloc_hits.saturating_add(1);
                inner.bytes_cached = inner.bytes_cached.saturating_sub(layout.size());
                inner.bytes_in_use = inner.bytes_in_use.saturating_add(layout.size());
                return ptr;
            }
        }

        inner.alloc_misses = inner.alloc_misses.saturating_add(1);
        inner.bytes_in_use = inner.bytes_in_use.saturating_add(layout.size());

        // 总持有字节数只会在 miss 时增长（走系统分配器）。
        let total = inner.bytes_in_use.saturating_add(inner.bytes_cached);
        inner.high_watermark_bytes = inner.high_watermark_bytes.max(total);

        // 注意：这里在持有 RefMut 时调用系统 alloc；
        // - 这是单线程池，且 alloc 失败会直接 handle_alloc_error abort；
        // - 保持实现简单，避免为“极小概率的 hook 重入”引入额外复杂度。
        let raw = unsafe { alloc(layout) };
        NonNull::new(raw).unwrap_or_else(|| handle_alloc_error(layout))
    }

    /// 回收一块 raw memory，供后续 alloc 复用。
    ///
    /// # Safety / Invariants（由调用方保证）
    /// - ptr 必须来自本 pool 的 alloc（或来自同一 Layout 的系统分配）；
    /// - layout 必须与该 ptr 的真实分配 Layout 完全一致（size/align）。
    pub(crate) fn recycle(&self, ptr: NonNull<u8>, layout: Layout) {
        if layout.size() == 0 {
            return;
        }

        let key = LayoutKey::new(layout);
        let mut inner = self.inner.borrow_mut();
        inner.recycle_calls = inner.recycle_calls.saturating_add(1);

        inner.bytes_in_use = inner.bytes_in_use.saturating_sub(layout.size());
        inner.bytes_cached = inner.bytes_cached.saturating_add(layout.size());

        inner.buckets.entry(key).or_default().push(ptr);
    }

    #[must_use]
    pub(crate) fn stats_snapshot(&self) -> AnchorPoolStatsSnapshot {
        let inner = self.inner.borrow();
        AnchorPoolStatsSnapshot {
            alloc_calls: inner.alloc_calls,
            alloc_hits: inner.alloc_hits,
            alloc_misses: inner.alloc_misses,
            recycle_calls: inner.recycle_calls,
            bytes_in_use: inner.bytes_in_use,
            bytes_cached: inner.bytes_cached,
            high_watermark_bytes: inner.high_watermark_bytes,
            bucket_count: inner.buckets.len(),
        }
    }
}

impl Drop for AnchorMemPool {
    fn drop(&mut self) {
        ////////////////////////////////////////////////////////////////////////////////
        // 释放池内缓存的 raw blocks。
        //
        // 说明：
        // - 仍在“in_use”的块理论上应在 Graph2 drop 时被 Node::drop 回收进 buckets；
        // - 即便极端情况下没能回收（例如进程 abort），这里也只负责清理已缓存部分。
        ////////////////////////////////////////////////////////////////////////////////
        let inner = self.inner.get_mut();
        for (key, mut blocks) in std::mem::take(&mut inner.buckets) {
            let Ok(layout) = Layout::from_size_align(key.size, key.align) else {
                // LayoutKey 来自 Layout，本不应失败；若失败，宁可泄漏也不冒险 dealloc。
                continue;
            };
            for ptr in blocks.drain(..) {
                unsafe {
                    dealloc(ptr.as_ptr(), layout);
                }
            }
        }
    }
}
