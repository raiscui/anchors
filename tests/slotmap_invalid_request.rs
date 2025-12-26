#![cfg(feature = "anchors_slotmap")]

use std::cell::Cell;
use std::rc::Rc;

use anchors::expert::{AnchorInner, Engine as EngineTrait, OutputContext, Poll, UpdateContext};
use anchors::singlethread::{Anchor, AnchorToken, Engine};

// ─────────────────────────────────────────────────────────────────────────────
// 说明：
// - 模拟“Anchor token 已失效”的异常场景，确保 request 不再 panic。
// - 该测试包含 unsafe，仅用于构造失效 token。
// ─────────────────────────────────────────────────────────────────────────────

struct StaleInputProbe {
    input: Anchor<u32>,
    output: u32,
    saw_pending: Rc<Cell<bool>>,
}

impl AnchorInner<Engine> for StaleInputProbe {
    type Output = u32;

    fn dirty(&mut self, _edge: &AnchorToken) {}

    fn poll_updated<G: UpdateContext<Engine = Engine>>(&mut self, ctx: &mut G) -> Poll {
        match ctx.request(&self.input, true) {
            Poll::Pending | Poll::PendingDefer => {
                self.saw_pending.set(true);
                self.output = 0;
                Poll::Updated
            }
            Poll::Updated | Poll::Unchanged => {
                self.output = *ctx.get(&self.input);
                Poll::Updated
            }
        }
    }

    fn output<'slf, 'out, G: OutputContext<'out, Engine = Engine>>(
        &'slf self,
        _ctx: &mut G,
    ) -> &'out Self::Output
    where
        'slf: 'out,
    {
        &self.output
    }

    fn debug_location(&self) -> Option<(&'static str, &'static std::panic::Location<'static>)> {
        None
    }
}

#[test]
fn request_missing_anchor_token_is_deferred() {
    let mut engine = Engine::new();
    let anchor = Anchor::constant(1u32);

    // ──────────────────────────────────────────────────────────────
    // unsafe: 使用 ptr::read 跳过 Clone 计数，制造“失效 token”。
    // 注意：仅用于测试异常路径，不可在业务代码中使用。
    // ──────────────────────────────────────────────────────────────
    let stale_anchor = unsafe { std::ptr::read(&anchor) };
    drop(anchor);

    // 触发一次 stabilize，确保 pending_free 被处理。
    engine.stabilize();

    let saw_pending = Rc::new(Cell::new(false));
    let probe = <Engine as EngineTrait>::mount(StaleInputProbe {
        input: stale_anchor,
        output: 0,
        saw_pending: saw_pending.clone(),
    });

    let out = engine.get(&probe);
    assert_eq!(out, 0);
    assert!(saw_pending.get());
}
