/*
 * @Description: 比较锁严格模式开关下的稳定化开销（编译特征 `lock_strict` 默认开启）。
 *  - 基准：简单两节点求和，反复 set + get。
 *  - 场景：strict=0 与 strict=1 各自独立基准，环境变量在基准前后切换回原值，避免交叉污染。
 *  - 若使用 `--no-default-features` 关闭 `lock_strict`，对应 strict=1 的基准会被跳过。
 */

use anchors::{
    expert::MultiAnchor,
    singlethread::{Engine, Var},
};
use criterion::{Criterion, black_box, criterion_group, criterion_main};

/// 单轮工作负载：set 一次、get 一次，触发稳定化。
fn workload(strict: bool, c: &mut Criterion) {
    let label = if strict {
        "lock_strict_on"
    } else {
        "lock_strict_off"
    };

    // 保存旧值，基准结束后恢复，避免影响其他基准或外部环境。
    let old_env = emg_debug_env::str_allow_empty("ANCHORS_LOCK_STRICT");
    // std 环境操作在本基准中可控，显式标记 unsafe 以通过编译检查。
    unsafe {
        std::env::set_var("ANCHORS_LOCK_STRICT", if strict { "1" } else { "0" });
    }

    // 若编译时关闭了 lock_strict 特征，严格模式无法生效，跳过对应基准以免误导。
    if strict && !cfg!(feature = "lock_strict") {
        eprintln!("lock_strict feature disabled，跳过 lock_strict_on 基准");
        return;
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
