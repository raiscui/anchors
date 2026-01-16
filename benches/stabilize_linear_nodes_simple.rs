use anchors::singlethread::{Anchor, Engine, Var};
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;

/// ════════════════════════════════════════════════════════════════════════
/// 节点规模组合，用 observed/unobserved 两种场景覆盖典型 workload。
const NODE_SCALES: [usize; 3] = [10, 100, 1000];
const OBSERVED_STATES: [bool; 2] = [true, false];

/// 针对“建链 + 重算”与“纯 get”分开度量，避免把一次性构建成本平均到每节点。
fn bench_split_paths(c: &mut Criterion) {
    let mut group = c.benchmark_group("stabilize_linear_nodes_split");
    // 为 100 与 1000 做细分
    for &(count, observed) in &[(100usize, true), (100, false), (1000, true), (1000, false)] {
        let label = if observed { "obs" } else { "unobs" };

        // 1) 建链 + 重算：把构建成本单独拿出来看（这部分通常只在 UI 初次创建时发生）。
        group.bench_function(
            BenchmarkId::new("build_plus_recalc", format!("{count}/{label}")),
            |b| {
                b.iter(|| black_box(run_single_chain(count, observed)));
            },
        );

        // 2) 纯 get：只测读取路径（无 set），用于观察 ready fast-path 是否生效。
        group.bench_function(
            BenchmarkId::new("get_only", format!("{count}/{label}")),
            |b| {
                let mut engine = Engine::new_with_max_height(count + 16);
                let (_source, tail) = build_linear_chain(&mut engine, count, observed);
                // 先跑一次，让节点进入 ready 状态（否则首轮会包含完整 stabilize）。
                black_box(engine.get(&tail));
                b.iter(|| black_box(engine.get(&tail)));
            },
        );

        // 3) 仅重算：只测 set + get 的传播路径（不重复建图），更接近“增量更新”的核心指标。
        group.bench_function(
            BenchmarkId::new("recalc_only", format!("{count}/{label}")),
            |b| {
                let mut engine = Engine::new_with_max_height(count + 16);
                let (source, tail) = build_linear_chain(&mut engine, count, observed);
                // 预热一次，避免把首次 stabilize 计入每次迭代。
                black_box(engine.get(&tail));

                let mut update_number: u64 = 0;
                b.iter(|| {
                    update_number = update_number.wrapping_add(1);
                    source.set(update_number);
                    black_box(engine.get(&tail))
                });
            },
        );
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
                // 为了和原版输出对齐，把路径做成 `.../1000/unobserved` 的顺序。
                BenchmarkId::new(node_count.to_string(), label),
                &(node_count, observed),
                |b, &(count, observed)| {
                    // 只测“set + get”的增量重算，不把建图成本混进每次迭代。
                    let mut engine = Engine::new_with_max_height(count + 16);
                    let (source, tail) = build_linear_chain(&mut engine, count, observed);
                    black_box(engine.get(&tail));

                    let mut update_number: u64 = 0;
                    b.iter(|| {
                        update_number = update_number.wrapping_add(1);
                        source.set(update_number);
                        black_box(engine.get(&tail))
                    });
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
    let baseline = black_box(engine.get(&tail));
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
        node = node.map(|value| value + black_box(1u64));
    }
    if observed {
        engine.mark_observed(&node);
    }
    (source, node)
}

criterion_group!(
    slotmap_benches,
    bench_stabilize_linear_nodes_simple,
    bench_split_paths
);
criterion_main!(slotmap_benches);
