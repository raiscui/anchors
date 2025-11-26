/*
 * @Description: 比较 anchors_slotmap 特征开关对基础稳定化开销的影响。
 *  - 标签：根据编译特征自动命名为 slotmap_on / slotmap_off，便于跨两次构建对比。
 *  - 用法：
 *      默认（slotmap 开启）：cargo bench -p anchors --bench slotmap_feature_overhead
 *      关闭特征对照：cargo bench -p anchors --no-default-features --features generic_size_voa,lock_strict --bench slotmap_feature_overhead
 *    将两次输出的中位值进行对比即可看到开关差异。
 */

use anchors::{
    expert::MultiAnchor,
    singlethread::{Engine, Var},
};
use criterion::{Criterion, black_box, criterion_group, criterion_main};

fn bench_slotmap_toggle(c: &mut Criterion) {
    let label = if cfg!(feature = "anchors_slotmap") {
        "slotmap_on"
    } else {
        "slotmap_off"
    };

    let mut group = c.benchmark_group("slotmap_feature_overhead");

    group.bench_function(label, |bencher| {
        // ── 基础工作负载：两个输入 Var 相加，反复 set + get。
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

    group.finish();
}

criterion_group!(slotmap_toggle, bench_slotmap_toggle);
criterion_main!(slotmap_toggle);
