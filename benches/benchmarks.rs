use anchors::{
    collections::ord_map_methods::Dict,
    expert::VarVOA,
    singlethread::{Anchor, Engine, Var},
};

use smallvec::{SmallVec, smallvec};

use anchors::im_rc::{Vector, ordmap, vector};
use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};

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

fn var_either_anchor3(c: &mut Criterion) {
    for node_count in &[10, 100, 1000] {
        for observed in &[true, false] {
            c.bench_with_input(
                BenchmarkId::new(
                    "var_either_anchor3",
                    format!(
                        "{}/{}",
                        node_count,
                        if *observed { "observed" } else { "unobserved" }
                    ),
                ),
                &(*node_count, *observed),
                |b, (node_count, observed)| {
                    let mut engine = Engine::new_with_max_height(2006);
                    let first_num = Var::new(0u64);
                    let mut node = first_num.watch();

                    for _ in 0..*node_count {
                        node = Var::new(node.map(|val| val + black_box(1)))
                            .watch()
                            .then(|x| x.clone());
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
fn var_either_anchor_def(c: &mut Criterion) {
    for node_count in &[10, 100, 1000] {
        for observed in &[true, false] {
            c.bench_with_input(
                BenchmarkId::new(
                    "var_either_anchor_def",
                    format!(
                        "{}/{}",
                        node_count,
                        if *observed { "observed" } else { "unobserved" }
                    ),
                ),
                &(*node_count, *observed),
                |b, (node_count, observed)| {
                    let mut engine = Engine::new_with_max_height(2006);
                    let first_num = VarVOA::new(0u64);
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
fn var_either_anchor_def_either(c: &mut Criterion) {
    for node_count in &[10, 100, 1000] {
        for observed in &[true, false] {
            c.bench_with_input(
                BenchmarkId::new(
                    "var_either_anchor_def_either",
                    format!(
                        "{}/{}",
                        node_count,
                        if *observed { "observed" } else { "unobserved" }
                    ),
                ),
                &(*node_count, *observed),
                |b, (node_count, observed)| {
                    let mut engine = Engine::new_with_max_height(2006);
                    let first_num = VarVOA::new(0u64);
                    let mut node = first_num.watch();

                    for _ in 0..*node_count {
                        node = node.either(|val| (val + black_box(1)).into());
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

fn var_either_anchor2(c: &mut Criterion) {
    for node_count in &[10, 100, 1000] {
        for observed in &[true, false] {
            c.bench_with_input(
                BenchmarkId::new(
                    "var_either_anchor2",
                    format!(
                        "{}/{}",
                        node_count,
                        if *observed { "observed" } else { "unobserved" }
                    ),
                ),
                &(*node_count, *observed),
                |b, (node_count, observed)| {
                    let mut engine = Engine::new_with_max_height(2006);
                    let first_num = Var::new(0u64);
                    let mut node = first_num.watch();

                    for _ in 0..*node_count {
                        node = VarVOA::new(node.map(|val| val + black_box(1))).watch();
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
fn var_either_anchor(c: &mut Criterion) {
    for node_count in &[10, 100, 1000] {
        for observed in &[true, false] {
            c.bench_with_input(
                BenchmarkId::new(
                    "var_either_anchor",
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
                        let node_x = VarVOA::new(1);
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

fn sm_vector_anchor(c: &mut Criterion) {
    for node_count in &[10, 100, 1000] {
        for observed in &[true, false] {
            c.bench_with_input(
                BenchmarkId::new(
                    "sm_vector_anchor",
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

                    let mut v: SmallVec<[Anchor<u64>; 11]> = smallvec![node];
                    for _ in 0..*node_count {
                        let node_x = Var::new(1);
                        v.push(node_x.watch());
                    }
                    let va: Anchor<SmallVec<[u64; 11]>> = v.into_iter().collect();

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
fn ord_map_anchor(c: &mut Criterion) {
    for node_count in &[10, 100, 1000] {
        for observed in &[true, false] {
            c.bench_with_input(
                BenchmarkId::new(
                    "ord_map_anchor",
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

                    let mut v = ordmap! {"-1".to_string()=> node};
                    for i in 0..*node_count {
                        let node_x = Var::new(1);
                        v.insert(i.to_string(), node_x.watch());
                    }
                    let va: Anchor<Dict<_, _>> = v.into_iter().collect();

                    let count_va: Anchor<u64> = va.map(|ns| ns.values().sum());
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

fn ord_map(c: &mut Criterion) {
    for node_count in &[10, 100, 1000] {
        {
            let observed = &false;
            c.bench_with_input(
                BenchmarkId::new(
                    "ord_map",
                    format!(
                        "{}/{}",
                        node_count,
                        if *observed { "observed" } else { "unobserved" }
                    ),
                ),
                &(*node_count, *observed),
                |b, (node_count, observed)| {
                    let first_num = 0u64;
                    let node = first_num;

                    let mut v = ordmap! {"-1".to_string()=> node};
                    for i in 0..*node_count {
                        let node_x = 1;
                        v.insert(i.to_string(), node_x);
                    }

                    let count_va: u64 = v.values().sum();

                    assert_eq!(count_va, *node_count);
                    // println!(
                    //     "node count: {}  count_va {}",
                    //     node_count,
                    //     engine.get(&count_va)
                    // );
                    let mut update_number = 0;
                    b.iter(|| {
                        update_number += 1;
                        *v.get_mut("-1").unwrap() = update_number;
                        let count_va: u64 = v.values().sum();

                        assert_eq!(count_va, update_number + *node_count);
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
    targets = vector_anchor,sm_vector_anchor
}
criterion_group! {
    name = benches_ordmap;
    config = Criterion::default();
    targets = ord_map_anchor
}

criterion_group! {
    name = all;
    config = Criterion::default();
    targets = stabilize_linear_nodes_cutoff, stabilize_linear_nodes_not_cutoff,stabilize_linear_nodes_simple,vector_anchor,sm_vector_anchor,ord_map_anchor
}

criterion_group! {
    name = im;
    config = Criterion::default();
    targets =vector_anchor,ord_map_anchor,ord_map

}
criterion_group! {
    name = vec;
    config = Criterion::default();
    targets =vector_anchor,var_either_anchor

}
criterion_group! {
    name = var_e_a;
    config = Criterion::default();
    targets =stabilize_linear_nodes_simple,var_either_anchor_def,var_either_anchor_def_either,var_either_anchor2,var_either_anchor3

}

criterion_group! {
    name = ord_map_a;
    config = Criterion::default();
    targets = ord_map_anchor

}

// criterion_main!(all);
// criterion_main!(im);
criterion_main!(benches);
