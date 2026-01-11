/*
 * @Description: 比较 anchors_pending_queue 特征开关对性能的影响。
 *
 * 说明：
 * - `anchors_pending_queue` 是编译期 feature，无法在同一次运行中动态切换；
 *   因此需要跑两次 `cargo bench`，再对比两次输出的中位值/方差。
 * - 该基准包含两类场景：
 *   1) no_pending：队列始终为空，主要测“每次 stabilize 末尾多一次检查/early-return”的固定开销。
 *   2) defer_case：构造一次“引入新依赖”的场景，触发 pending（开启 feature 时会入队 + drain），
 *      主要测“入队/去重 + drain/retry”的额外开销。
 *
 * 用法：
 * - 默认（pending queue 关闭）：
 *     cargo bench -p anchors --bench pending_queue_feature_overhead
 * - 显式开启对照：
 *     cargo bench -p anchors --features anchors_pending_queue --bench pending_queue_feature_overhead
 *
 * 对比方式：
 * - 两次运行的 label 会分别带上 `pending_queue_off/<case>` 与 `pending_queue_on/<case>` 前缀；
 *   直接对比对应项即可。
 */

use anchors::{
    expert::MultiAnchor,
    expert::{AnchorInner, Engine as AnchorEngine, OutputContext, Poll, UpdateContext},
    singlethread::{Anchor, AnchorToken, Engine, Var},
};
use criterion::{BatchSize, Criterion, black_box, criterion_group, criterion_main};

/// 构造一个“按开关决定是否请求 child”的 AnchorInner。
///
/// 设计目标：
/// - enable=false 时不 request child，输出为 0；
/// - enable=true 时 request deep child，并在首次引入依赖时触发 pending（用于观测 pending queue 开关差异）。
struct ToggleChildAnchor {
    enable: Anchor<bool>,
    child: Anchor<usize>,
    output: Option<usize>,
    output_stale: bool,
}

impl ToggleChildAnchor {
    fn new(enable: Anchor<bool>, child: Anchor<usize>) -> Self {
        Self {
            enable,
            child,
            output: None,
            output_stale: true,
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

        // 先 request enable，保证 enable 变化能驱动该节点重算。
        match ctx.request(&self.enable, true) {
            Poll::Updated | Poll::Unchanged => {}
            Poll::Pending => return Poll::Pending,
            #[cfg(feature = "anchors_pending_queue")]
            Poll::PendingDefer => return Poll::Pending,
            Poll::PendingInvalidToken => return Poll::Pending,
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

        // enable=true 时才 request child，用于制造“首次引入依赖”的场景。
        match ctx.request(&self.child, true) {
            Poll::Updated | Poll::Unchanged => {}
            Poll::Pending => return Poll::Pending,
            #[cfg(feature = "anchors_pending_queue")]
            Poll::PendingDefer => return Poll::Pending,
            Poll::PendingInvalidToken => return Poll::Pending,
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

struct DeferCaseState {
    engine: Engine,
    enable: Var<bool>,
    toggle: Anchor<usize>,
}

impl DeferCaseState {
    fn new() -> Self {
        let mut engine = Engine::new();

        let enable = Var::new(false);
        let child_input = Var::new(5usize);

        // 提升 child 高度（加深依赖链），更容易触发“首次引入依赖时需要回填高度/重排”的 pending。
        let deep_child = child_input
            .watch()
            .map(|v| v + 1)
            .map(|v| v + 1)
            .map(|v| v + 1);

        let toggle: Anchor<usize> =
            AnchorEngine::mount(ToggleChildAnchor::new(enable.watch(), deep_child));

        // 先跑一轮，让 enable=false 的分支输出 ready，避免把“首次构建图”的成本混进 defer_case。
        black_box(engine.get(&toggle));

        Self {
            engine,
            enable,
            toggle,
        }
    }
}

fn bench_pending_queue_toggle(c: &mut Criterion) {
    let label_no_pending = if cfg!(feature = "anchors_pending_queue") {
        "pending_queue_on/no_pending"
    } else {
        "pending_queue_off/no_pending"
    };
    let label_defer_case = if cfg!(feature = "anchors_pending_queue") {
        "pending_queue_on/defer_case"
    } else {
        "pending_queue_off/defer_case"
    };

    let mut group = c.benchmark_group("pending_queue_feature_overhead");

    // ─────────────────────────────────────────────────────────────
    // no_pending：队列为空的常态开销
    group.bench_function(label_no_pending, |bencher| {
        let mut engine = Engine::new();
        let a = Var::new(1u32);
        let b = Var::new(2u32);
        let aw = a.watch();
        let bw = b.watch();
        let sum = (&aw, &bw).map(|x, y| *x + *y);

        let mut tick = 0u32;
        bencher.iter(|| {
            tick = tick.wrapping_add(1);
            a.set(tick);
            b.set(tick.wrapping_mul(2));
            black_box(engine.get(&sum));
        });
    });

    // ─────────────────────────────────────────────────────────────
    // defer_case：构造一次“引入新依赖”触发 pending 的路径
    //
    // 这里使用 `iter_batched_ref`，确保：
    // - setup（构造 Engine/Anchor 图）的成本不计入测量；
    // - 输入状态的 drop 也不被计入（避免释放成本污染结果）。
    group.bench_function(label_defer_case, |bencher| {
        bencher.iter_batched_ref(
            DeferCaseState::new,
            |state| {
                state.enable.set(true);
                black_box(state.engine.get(&state.toggle));
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

criterion_group!(pending_queue_toggle, bench_pending_queue_toggle);
criterion_main!(pending_queue_toggle);
