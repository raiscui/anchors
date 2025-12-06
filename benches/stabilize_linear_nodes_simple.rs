use anchors::singlethread::{Anchor, Engine, Var};
use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};

/// ════════════════════════════════════════════════════════════════════════
/// 节点规模组合，用 observed/unobserved 两种场景覆盖典型 workload。
const NODE_SCALES: [usize; 3] = [10, 100, 1000];
const OBSERVED_STATES: [bool; 2] = [true, false];

/// 针对“建链 + 重算”与“纯 get”分开度量，避免把一次性构建成本平均到每节点。
fn bench_split_paths(c: &mut Criterion) {
    let mut group = c.benchmark_group("stabilize_linear_nodes_split");
    // 为 100 与 1000 做细分
    for &(count, observed) in &[(100usize, true), (100, false), (1000, true), (1000, false)] {
        let label_build = format!(
            "build_once_get/{count}/{}",
            if observed { "obs" } else { "unobs" }
        );
        group.bench_function(BenchmarkId::new("build_once_get", &label_build), |b| {
            b.iter(|| run_build_once_get(count, observed));
        });

        let label_recalc = format!(
            "recalc_only/{count}/{}",
            if observed { "obs" } else { "unobs" }
        );
        group.bench_function(BenchmarkId::new("recalc_only", &label_recalc), |b| {
            b.iter(|| run_recalc_only(count, observed));
        });
    }
    group.finish();
}

fn bench_stabilize_linear_nodes_simple(c: &mut Criterion) {
    let mut group = c.benchmark_group("stabilize_linear_nodes_simple");
    for &node_count in NODE_SCALES.iter() {
        group.throughput(Throughput::Elements(node_count as u64));
        for &observed in OBSERVED_STATES.iter() {
            let label = if observed { "observed" } else { "unobserved" };
            group.bench_with_input(
                BenchmarkId::new(label, node_count),
                &(node_count, observed),
                |b, &(count, observed)| {
                    b.iter(|| run_single_chain(count, observed));
                },
            );
        }
    }
    group.finish();
}

/// ════════════════════════════════════════════════════════════════════════
/// 真正执行一次“构建链→stabilize→写入→再次 stabilize”的循环，并返回最终值。
fn run_single_chain(node_count: usize, observed: bool) -> u64 {
    let mut engine = Engine::new_with_max_height(node_count + 16);
    let (source, tail) = build_linear_chain(&mut engine, node_count, observed);
    // 先确保初始取值无偏差，避免优化器省略。
    let mut baseline = engine.get(&tail);
    baseline = black_box(baseline);
    source.set(baseline + 1);
    black_box(engine.get(&tail))
}

/// ════════════════════════════════════════════════════════════════════════
/// 构造单链路，返回 (源 Var, 末端 Anchor)。
fn build_linear_chain(
    engine: &mut Engine,
    node_count: usize,
    observed: bool,
) -> (Var<u64>, Anchor<u64>) {
    let source = Var::new(0u64);
    let mut node = source.watch();
    for _ in 0..node_count {
        node = node.map(|value| value + 1);
    }
    if observed {
        engine.mark_observed(&node);
    }
    (source, node)
}

/// 一次构建，随后仅 get，测读取路径；避免把建图成本平均到节点。
fn run_build_once_get(node_count: usize, observed: bool) -> u64 {
    let mut engine = Engine::new_with_max_height(node_count + 16);
    let (source, tail) = build_linear_chain(&mut engine, node_count, observed);
    engine.get(&tail);
    // 变更源，触发一次稳定
    source.set(1);
    engine.get(&tail)
}

/// 构建后仅反复 set + stabilize，度量重算链路，不重新建图。
fn run_recalc_only(node_count: usize, observed: bool) -> u64 {
    let mut engine = Engine::new_with_max_height(node_count + 16);
    let (source, tail) = build_linear_chain(&mut engine, node_count, observed);
    engine.get(&tail);
    source.set(1);
    engine.get(&tail)
}

criterion_group!(
    slotmap_benches,
    bench_stabilize_linear_nodes_simple,
    bench_split_paths
);
criterion_main!(slotmap_benches);
