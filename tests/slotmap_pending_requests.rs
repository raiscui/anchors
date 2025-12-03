#![cfg(feature = "anchors_slotmap")]

use std::{cell::RefCell, rc::Rc};

use anchors::expert::{AnchorInner, Engine as AnchorEngine, OutputContext, Poll, UpdateContext};
use anchors::singlethread::{Anchor, AnchorToken, Engine, Var};

/// 切换依赖的探针 Anchor ，用于验证 PendingDefer → drain → Updated 的完整流程。
struct ToggleChildAnchor {
    enable: Anchor<bool>,
    child: Anchor<usize>,
    output: Option<usize>,
    output_stale: bool,
    poll_records: Rc<RefCell<Vec<Poll>>>,
}

impl ToggleChildAnchor {
    fn new(
        enable: Anchor<bool>,
        child: Anchor<usize>,
        poll_records: Rc<RefCell<Vec<Poll>>>,
    ) -> Self {
        Self {
            enable,
            child,
            output: None,
            output_stale: true,
            poll_records,
        }
    }

    fn mark_stale(&mut self, token: &AnchorToken) {
        if token == &self.enable.token() || token == &self.child.token() {
            self.output_stale = true;
        }
    }
}

impl AnchorInner<Engine> for ToggleChildAnchor {
    type Output = usize;

    fn dirty(&mut self, child: &AnchorToken) {
        self.mark_stale(child);
    }

    fn poll_updated<G: UpdateContext<Engine = Engine>>(&mut self, ctx: &mut G) -> Poll {
        if !self.output_stale && self.output.is_some() {
            return Poll::Unchanged;
        }

        match ctx.request(&self.enable, true) {
            Poll::Pending | Poll::PendingDefer => return Poll::Pending,
            _ => {}
        }
        let enabled = *ctx.get(&self.enable);
        if !enabled {
            let changed = self.output != Some(0);
            self.output = Some(0);
            self.output_stale = false;
            return if changed {
                Poll::Updated
            } else {
                Poll::Unchanged
            };
        }

        let poll = ctx.request(&self.child, true);
        let is_pending = matches!(poll, Poll::Pending | Poll::PendingDefer);
        self.poll_records.borrow_mut().push(poll);
        if is_pending {
            return Poll::Pending;
        }

        let next_val = *ctx.get(&self.child) + 1;
        let changed = self.output != Some(next_val);
        self.output = Some(next_val);
        self.output_stale = false;
        if changed {
            Poll::Updated
        } else {
            Poll::Unchanged
        }
    }

    fn output<'slf, 'out, G: OutputContext<'out, Engine = Engine>>(
        &'slf self,
        _ctx: &mut G,
    ) -> &'out Self::Output
    where
        'slf: 'out,
    {
        self.output
            .as_ref()
            .expect("toggle anchor output must be ready")
    }
}

#[test]
fn pending_queue_defers_new_dependency_until_ready() {
    let mut engine = Engine::new();
    let enable = Var::new(false);
    let child_input = Var::new(5usize);

    // 提升 child 高度，确保首次依赖需要回填高度 → 触发 PendingDefer。
    let deep_child = child_input
        .watch()
        .map(|v| v + 1)
        .map(|v| v + 1)
        .map(|v| v + 1);

    let records = Rc::new(RefCell::new(Vec::new()));
    let toggle_anchor: Anchor<usize> = AnchorEngine::mount(ToggleChildAnchor::new(
        enable.watch(),
        deep_child,
        records.clone(),
    ));

    assert_eq!(engine.get(&toggle_anchor), 0);

    enable.set(true);
    records.borrow_mut().clear();
    let value = engine.get(&toggle_anchor);
    assert_eq!(value, 9, "child(5)+3 层 map + 探针加 1 = 9");

    let log = records.borrow();
    assert!(
        matches!(log.first(), Some(Poll::PendingDefer)),
        "首次 request 应进入 PendingDefer，实际记录: {:?}",
        *log
    );
    assert!(
        log.len() >= 2 && matches!(log.last(), Some(Poll::Updated | Poll::Unchanged)),
        "第二轮 drain 后应落在 Updated/Unchanged，实际记录: {:?}",
        *log
    );

    let stats = engine.pending_stats_snapshot();
    assert!(stats.total_enqueued_unique >= 1);
    assert_eq!(stats.total_enqueued_unique, stats.total_drained);
    assert!(stats.max_queue_len >= 1);
    assert_eq!(stats.last_drain_remaining, 0);
}
