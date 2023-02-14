use anchors::singlethread::{Anchor, Engine, MultiAnchor, Var};
use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use im_rc::{vector, Vector};

fn stabilize_linear_nodes_simple(c: &mut Criterion) {
    for node_count in &[10, 100, 1000] {
        for observed in &[true, false] {
            c.bench_with_input(
                BenchmarkId::new(
                    "stabilize_linear_nodes_simple",
                    format!(
                        "{}/{}",
                        node_count,
                        if *observed { "observed" } else { "unobserved" }
                    ),
                ),
                &(*node_count, *observed),
                |b, (node_count, observed)| {
                    let mut engine = Engine::new_with_max_height(1003);
                    let first_num = Var::new(0u64);
                    let mut node = first_num.watch();
                    for _ in 0..*node_count {
                        node = node.map(|val| val + black_box(1));
                    }
                    if *observed {
                        engine.mark_observed(&node);
                    }
                    assert_eq!(engine.get(&node), *node_count);
                    let mut update_number = 0;
                    b.iter(|| {
                        update_number += 1;
                        first_num.set(update_number);
                        assert_eq!(engine.get(&node), update_number + *node_count);
                    });
                },
            );
        }
    }
}

fn stabilize_linear_nodes_cutoff(c: &mut Criterion) {
    for node_count in &[10, 100, 1000] {
        for observed in &[true, false] {
            c.bench_with_input(
                BenchmarkId::new(
                    "stabilize_linear_nodes_cutoff",
                    format!(
                        "{}/{}",
                        node_count,
                        if *observed { "observed" } else { "unobserved" }
                    ),
                ),
                &(*node_count, *observed),
                |b, (node_count, observed)| {
                    let mut engine = Engine::new_with_max_height(1003);
                    let first_num = Var::new(0u64);
                    let node = first_num.watch();
                    let node = node.map(|val| black_box(val) - black_box(val) + 1);
                    let mut node = {
                        let mut old_val = None;
                        node.cutoff(move |val| {
                            if Some(*val) != old_val {
                                old_val = Some(*val);
                                true
                            } else {
                                false
                            }
                        })
                    };
                    for i in 0..*node_count {
                        node = node.map(move |val| black_box(val) - black_box(val) + black_box(i));
                    }
                    if *observed {
                        engine.mark_observed(&node);
                    }
                    assert_eq!(engine.get(&node), *node_count - 1);
                    let mut update_number = 0;
                    b.iter(|| {
                        update_number += 1;
                        first_num.set(update_number);
                        assert_eq!(engine.get(&node), *node_count - 1);
                    });
                },
            );
        }
    }
}

fn stabilize_linear_nodes_not_cutoff(c: &mut Criterion) {
    for node_count in &[10, 100, 1000] {
        for observed in &[true, false] {
            c.bench_with_input(
                BenchmarkId::new(
                    "stabilize_linear_nodes_not_cutoff",
                    format!(
                        "{}/{}",
                        node_count,
                        if *observed { "observed" } else { "unobserved" }
                    ),
                ),
                &(*node_count, *observed),
                |b, (node_count, observed)| {
                    let mut engine = Engine::new_with_max_height(1003);
                    let first_num = Var::new(0u64);
                    let node = first_num.watch();
                    let mut node = node.map(|val| black_box(val) - black_box(val) + 1);
                    // let mut node = {
                    //     let mut old_val = None;
                    //     node.cutoff(move |val| {
                    //         if Some(*val) != old_val {
                    //             old_val = Some(*val);
                    //             true
                    //         } else {
                    //             false
                    //         }
                    //     })
                    // };
                    for i in 0..*node_count {
                        node = node.map(move |val| black_box(val) - black_box(val) + black_box(i));
                    }
                    if *observed {
                        engine.mark_observed(&node);
                    }
                    assert_eq!(engine.get(&node), *node_count - 1);
                    let mut update_number = 0;
                    b.iter(|| {
                        update_number += 1;
                        first_num.set(update_number);
                        assert_eq!(engine.get(&node), *node_count - 1);
                    });
                },
            );
        }
    }
}

fn vector_anchor(c: &mut Criterion) {
    for node_count in &[10, 100, 1000] {
        for observed in &[true, false] {
            c.bench_with_input(
                BenchmarkId::new(
                    "vector_anchor",
                    format!(
                        "{}/{}",
                        node_count,
                        if *observed { "observed" } else { "unobserved" }
                    ),
                ),
                &(*node_count, *observed),
                |b, (node_count, observed)| {
                    let mut engine = Engine::new_with_max_height(1003);
                    let first_num = Var::new(0u64);
                    let node = first_num.watch();

                    let mut v = vector![node];
                    for _ in 0..*node_count {
                        let node_x = Var::new(1);
                        v.push_back(node_x.watch());
                    }
                    let va: Anchor<Vector<_>> = v.into_iter().collect();

                    let count_va: Anchor<u64> = va.map(|ns| ns.iter().sum());
                    if *observed {
                        engine.mark_observed(&count_va);
                    }
                    assert_eq!(engine.get(&count_va), *node_count);
                    // println!(
                    //     "node count: {}  count_va {}",
                    //     node_count,
                    //     engine.get(&count_va)
                    // );
                    let mut update_number = 0;
                    b.iter(|| {
                        update_number += 1;
                        first_num.set(update_number);
                        assert_eq!(engine.get(&count_va), update_number + *node_count);
                    });
                },
            );
        }
    }
}

criterion_group! {
    name = benches;
    config = Criterion::default();
    targets = stabilize_linear_nodes_cutoff, stabilize_linear_nodes_not_cutoff,stabilize_linear_nodes_simple
}
criterion_group! {
    name = benches_vector;
    config = Criterion::default();
    targets = vector_anchor
}
criterion_main!(benches_vector);
