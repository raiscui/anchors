//! then_dedupe 单入参去重行为验证：
//! - 相同输入值应复用旧输出 Anchor（token 不变，避免高度回填）。
//! - 输入值变更时应切换新输出 Anchor（token 改变）。

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use anchors::singlethread::{Anchor, Engine, Var};

#[test]
fn reuse_on_same_input() {
    let mut engine = Engine::new();
    let source = Var::new(1u32);

    // then_dedupe: 输入未变时复用旧 Anchor，避免频繁切换 token。
    let call_cnt = Arc::new(AtomicUsize::new(0));
    let call_cnt_closure = call_cnt.clone();
    let out = source.watch().then_dedupe(move |v| {
        call_cnt_closure.fetch_add(1, Ordering::SeqCst);
        Anchor::constant(*v + 10)
    });

    assert_eq!(engine.get(&out), 11);
    assert_eq!(call_cnt.load(Ordering::SeqCst), 1, "首次应调用一次闭包");

    // 同值更新，不应再次执行闭包（应复用旧输出 Anchor）。
    source.set(1);
    assert_eq!(engine.get(&out), 11);
    assert_eq!(
        call_cnt.load(Ordering::SeqCst),
        1,
        "相同输入应复用旧输出 Anchor，不应重复调用闭包"
    );

    // 不同值更新，应重新执行闭包并生成新输出 Anchor。
    source.set(2);
    assert_eq!(engine.get(&out), 12);
    assert_eq!(
        call_cnt.load(Ordering::SeqCst),
        2,
        "输入变化时应重新执行闭包生成新输出 Anchor"
    );
}

#[test]
fn reuse_on_same_input_two_args() {
    let mut engine = Engine::new();
    let a = Var::new(1u32);
    let b = Var::new(10u32);

    let call_cnt = Arc::new(AtomicUsize::new(0));
    let call_cnt_closure = call_cnt.clone();
    let out = a.watch().then_dedupe2(&b.watch(), move |x, y| {
        call_cnt_closure.fetch_add(1, Ordering::SeqCst);
        Anchor::constant(*x + *y)
    });

    assert_eq!(engine.get(&out), 11);
    assert_eq!(call_cnt.load(Ordering::SeqCst), 1);

    // 同值更新，不应触发闭包。
    a.set(1);
    b.set(10);
    assert_eq!(engine.get(&out), 11);
    assert_eq!(call_cnt.load(Ordering::SeqCst), 1);

    // 变更其中一个输入，应触发闭包。
    b.set(20);
    assert_eq!(engine.get(&out), 21);
    assert_eq!(call_cnt.load(Ordering::SeqCst), 2);
}

#[test]
fn reuse_on_same_input_three_args() {
    let mut engine = Engine::new();
    let a = Var::new(1u32);
    let b = Var::new(2u32);
    let c = Var::new(3u32);

    let call_cnt = Arc::new(AtomicUsize::new(0));
    let call_cnt_closure = call_cnt.clone();
    let out = a
        .watch()
        .then_dedupe3(&b.watch(), &c.watch(), move |x, y, z| {
            call_cnt_closure.fetch_add(1, Ordering::SeqCst);
            Anchor::constant(*x + *y + *z)
        });

    assert_eq!(engine.get(&out), 6);
    assert_eq!(call_cnt.load(Ordering::SeqCst), 1);

    // 同值更新，不应触发闭包。
    b.set(2);
    c.set(3);
    assert_eq!(engine.get(&out), 6);
    assert_eq!(call_cnt.load(Ordering::SeqCst), 1);

    // 修改任一输入，应触发闭包。
    c.set(4);
    assert_eq!(engine.get(&out), 7);
    assert_eq!(call_cnt.load(Ordering::SeqCst), 2);
}
