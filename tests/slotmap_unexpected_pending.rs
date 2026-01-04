#![cfg(feature = "anchors_slotmap")]

use anchors::expert::{
    AnchorHandle, AnchorInner, Engine as AnchorEngine, OutputContext, Poll, UpdateContext,
};
use anchors::singlethread::{Anchor, Engine, Var};
use std::panic::Location;

type Token = <<Engine as AnchorEngine>::AnchorHandle as AnchorHandle>::Token;

/// ////////////////////////////////////////////////////////////////////////////
/// 探针：故意制造“不符合 Anchors 契约”的 Pending 场景
///
/// 目标：
/// - 模拟某个节点在已经有历史输出后，`poll_updated` 返回 Pending，
///   但本次 poll 没有 request 任何未 Ready 子 Anchor（pending_on_anchor_get=false）。
/// - 引擎应当冻结旧输出并避免 panic / stabilize 自旋。
///
/// 说明：
/// - 这不是“推荐写法”，而是用于验证引擎兜底逻辑的回归测试。
/// ////////////////////////////////////////////////////////////////////////////
struct PendingWithoutRequestAfterReady {
    input: Anchor<usize>,
    output: usize,
    first_poll: bool,
    output_stale: bool,
    location: &'static Location<'static>,
}

impl AnchorInner<Engine> for PendingWithoutRequestAfterReady {
    type Output = usize;

    fn dirty(&mut self, edge: &Token) {
        if edge == &self.input.token() {
            self.output_stale = true;
        }
    }

    fn poll_updated<G: UpdateContext<Engine = Engine>>(&mut self, ctx: &mut G) -> Poll {
        // 第一次 poll：建立依赖并产出初始输出，确保后续有“历史输出”可冻结。
        if self.first_poll {
            match ctx.request(&self.input, true) {
                Poll::Pending | Poll::PendingDefer => return Poll::Pending,
                Poll::PendingInvalidToken => return Poll::PendingInvalidToken,
                Poll::Updated | Poll::Unchanged => {}
            }
            self.output = *ctx.get(&self.input);
            self.first_poll = false;
            self.output_stale = false;
            return Poll::Updated;
        }

        // 输入变更后：故意违规 —— 不 request 任何子 Anchor，直接返回 Pending。
        if self.output_stale {
            self.output_stale = false;
            return Poll::Pending;
        }

        Poll::Unchanged
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

    fn debug_location(&self) -> Option<(&'static str, &'static Location<'static>)> {
        Some(("PendingWithoutRequestAfterReady", self.location))
    }
}

#[test]
fn unexpected_pending_should_freeze_old_output_instead_of_panicking() {
    let mut engine = Engine::new();
    let input = Var::new(1usize);

    let anchor: Anchor<usize> = AnchorEngine::mount(PendingWithoutRequestAfterReady {
        input: input.watch(),
        output: 0,
        first_poll: true,
        output_stale: true,
        location: Location::caller(),
    });

    assert_eq!(engine.get(&anchor), 1);

    // 触发 dirty，进入“违规 Pending”分支。
    input.set(2);

    // 关键断言：不 panic，并且冻结旧输出（仍返回 1）。
    assert_eq!(engine.get(&anchor), 1);
}
