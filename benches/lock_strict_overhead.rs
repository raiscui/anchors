/*
 * @Description: 比较 ANCHORS_LOCK_STRICT 开关下的稳定化开销。
 *  - 基准：简单两节点求和，反复 set + get。
 *  - 场景：strict=0 与 strict=1 各自独立基准，环境变量在基准前后切换回原值，避免交叉污染。
 */

use anchors::{
    expert::MultiAnchor,
    singlethread::{Engine, Var},
};
use criterion::{criterion_group, criterion_main, Criterion, black_box};

/// 单轮工作负载：set 一次、get 一次，触发稳定化。
fn workload(strict: bool, c: &mut Criterion) {
    let label = if strict { "lock_strict_on" } else { "lock_strict_off" };

    // 保存旧值，基准结束后恢复，避免影响其他基准或外部环境。
    let old_env = std::env::var("ANCHORS_LOCK_STRICT").ok();
    // std 环境操作在本基准中可控，显式标记 unsafe 以通过编译检查。
    unsafe {
        std::env::set_var("ANCHORS_LOCK_STRICT", if strict { "1" } else { "0" });
    }

    let mut group = c.benchmark_group("lock_strict_overhead");
    // 保持基准配置来源于命令行参数（Criterion 默认 sample_size=100、measurement_time=5s），
    // 如需缩短或放大测试规模，可在 `cargo bench -- --sample-size N --measurement-time S` 中覆盖。

    group.bench_function(label, |bencher| {
        let mut engine = Engine::new();
        let a = Var::new(1u32);
        let vb = Var::new(2u32);
        let aw = a.watch();
        let bw = vb.watch();
        let sum = (&aw, &bw).map(|x, y| *x + *y);

        let mut tick = 0u32;
        bencher.iter(|| {
            tick = tick.wrapping_add(1);
            a.set(tick);
            vb.set(tick.wrapping_mul(2));
            black_box(engine.get(&sum));
        });
    });

    group.finish();

    match old_env {
        Some(v) => unsafe { std::env::set_var("ANCHORS_LOCK_STRICT", v) },
        None => unsafe { std::env::remove_var("ANCHORS_LOCK_STRICT") },
    }
}

fn benches(c: &mut Criterion) {
    workload(false, c);
    workload(true, c);
}

criterion_group!(lock_strict, benches);
criterion_main!(lock_strict);
