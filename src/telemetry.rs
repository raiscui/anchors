//! SlotMap 相关的观测工具，统一封装 fastrace span 与计数。
#![allow(dead_code)]

#[derive(Clone, Copy, Debug)]
pub enum SlotmapEventKind {
    Alloc,
    Free,
    GcSkipped,
    FreeSkip,
}

fn span_name(kind: SlotmapEventKind) -> &'static str {
    match kind {
        SlotmapEventKind::Alloc => "anchors.slotmap.alloc",
        SlotmapEventKind::Free => "anchors.slotmap.free",
        SlotmapEventKind::GcSkipped => "anchors.slotmap.gc_skipped",
        SlotmapEventKind::FreeSkip => "anchors.slotmap.free_skip",
    }
}

/// 记录一次 slotmap 事件，并附带最新计数，便于 Jaeger 中追踪趋势。
pub fn record_slotmap_event(kind: SlotmapEventKind, total: u64) {
    #[cfg(feature = "anchors_slotmap")]
    {
        use fastrace::prelude::*;

        let span = fastrace::Span::enter_with_local_parent(span_name(kind));
        span.add_property(move || ("total", total.to_string()));
        drop(span);
    }
    #[cfg(not(feature = "anchors_slotmap"))]
    {
        let _ = (kind, total);
    }
}
