use std::cell::RefCell;

use emg_common::Vector;

use crate::singlethread::{Anchor, Engine, MultiAnchor, Var};
thread_local! {
    pub static ENGINE: RefCell<Engine> = RefCell::new(Engine::new());
}

thread_local! {
    static G_ANIMA_RUNNING_STATE: Var<Vector<Anchor<bool>>> = Var::new(Vector::new());
}

fn global_anima_running_add(running: Anchor<bool>) {
    G_ANIMA_RUNNING_STATE.with(|sv| {
        let mut v = (*sv.get()).clone();

        v.push_back(running);
        sv.set(v);
    });
}

fn global_anima_running_build(_engine: &mut Engine) -> Anchor<bool> {
    let watch: Anchor<Vector<bool>> = G_ANIMA_RUNNING_STATE.with(|am| am.watch().into());
    let running: Anchor<bool> = watch.map(|list: &Vector<bool>| list.contains(&true));
    // e.mark_observed(&running);

    running
}

#[test]
fn test_observed_call() {
    let mut engine = Engine::new();

    let g = global_anima_running_build(&mut engine);

    let cat_count = Var::new(1);
    let dog_count = Var::new(1);
    let fish_count = Var::new(1);
    let total_mammals = (&cat_count.watch(), &dog_count.watch()).map(|cats, dogs| cats + dogs);
    let total_animals = (&total_mammals, &fish_count.watch()).map(|mammals, fish| mammals + fish);
    let mammal_callback = total_mammals.map(|total_mammals| {
        println!("mammals updated: {:?}", total_mammals);
        false
    });
    let animal_callback = total_animals.map(|total_animals| {
        println!("animals updated: {:?}", total_animals);
        false
    });

    global_anima_running_add(mammal_callback);
    global_anima_running_add(animal_callback);

    println!("running? {}", engine.get(&g));
    fish_count.set(2);
    println!("running? {}", engine.get(&g));
}

#[test]
fn test_mark_observed() {
    let mut engine = Engine::new();
    let anchor1 = Var::new(1);
    let anchor2 = Var::new(2);
    // let anchor3 = Var::new(anchor1.get() + anchor2.get());
    let anchor3 = (&anchor1.watch(), &anchor2.watch()).map(|a, b| a + b);
    {
        let dirty_marks = engine.dirty_marks.borrow();
        assert_eq!(dirty_marks.len(), 0);
    }

    engine.mark_observed(&anchor3);
    engine.stabilize();

    assert_eq!(engine.dirty_marks.borrow().len(), 0);
}

#[test]
fn update_dirty_marks_should_not_drop_vec_capacity() {
    ////////////////////////////////////////////////////////////////////////////////
    // 回归测试：锁定 dirty_marks 的 scratch 复用策略
    //
    // 背景：
    // - 旧实现使用 `mem::take(&mut dirty_marks)` 会把 dirty_marks 替换成全新 Vec(容量=0)；
    // - 在高频 set/dirty 场景下,这会导致 dirty_marks 每轮都重新扩容,在 pprof 中放大为
    //   `alloc::raw_vec::RawVecInner::finish_grow` 热点。
    //
    // 期望：
    // - `update_dirty_marks()` 消费队列后,dirty_marks 的 capacity 应保留,避免下一轮 push 再分配。
    ////////////////////////////////////////////////////////////////////////////////
    let mut engine = Engine::new();

    // 用一个稳定 token 反复填充 dirty_marks,只为了撑大 capacity。
    let v = Var::new(1u32);
    let watch = v.watch();
    let token = watch.token();

    {
        // 先把容量拉大,确保能观察到“是否被重置为 0”。
        let mut marks = engine.dirty_marks.borrow_mut();
        marks.reserve(1024);
        let cap_before = marks.capacity();
        marks.extend(std::iter::repeat(token).take(512));
        drop(marks);

        // 只跑 update_dirty_marks: 它会 drain 本轮 dirty_marks,并把节点入队,但不会 stabilize。
        engine.update_dirty_marks();

        // dirty_marks 作为“可继续 push 的共享队列”,应当保持容量,不能退化回 0。
        let marks_after = engine.dirty_marks.borrow();
        assert_eq!(marks_after.len(), 0);
        assert!(
            marks_after.capacity() >= cap_before,
            "dirty_marks capacity 被意外丢弃: before={cap_before} after={}",
            marks_after.capacity()
        );
    }
}

#[test]
fn stabilize_consumes_dirty_marks_created_during_stabilize() {
    ////////////////////////////////////////////////////////////////////////////////
    // 回归测试：锁定“方案A”的关键语义
    //
    // 现象：
    // - 某些 Anchor 在 poll_updated 期间会调用外部 `Var::set`（例如 CSS shaper 写入 layout.w/h）。
    // - 这些 set 只会把 dirty_marks 追加进队列；如果 stabilize 只跑一轮，
    //   那么本次 `Engine::get()` 可能返回“中间态”（需要下一次 get 才能看到最终值）。
    //
    // 期望：
    // - `Engine::stabilize()` 需要在同一次 get 内继续吃掉 stabilize 期间新增的 dirty_marks，
    //   直到收敛，从而返回最终值。
    ////////////////////////////////////////////////////////////////////////////////
    let mut engine = Engine::new();

    // 1) 被写入的目标 Var（模拟 layout.w/h）
    let width = Var::new(0usize);

    // 2) 触发器 Var：当它变为 true 时，在 map 闭包里写入 width
    let trigger = Var::new(false);

    // 3) 触发器 Anchor：在 stabilize 期间执行 side-effect（写入 width）
    let trigger_anchor = trigger.watch().map({
        let width = width.clone();
        move |flag: &bool| {
            if *flag {
                width.set(1);
            }
            // 返回值本身不重要；我们只用它来保证该 Anchor 会被计算。
            0usize
        }
    });

    // 4) 输出 Anchor：读取 width 的值（我们断言第二次 get 就应当看到 1）
    let out = (&width.watch(), &trigger_anchor).map(|w, _| *w);

    // 首次 get：建立 dirty_handle（否则 set 不会 mark_dirty）
    assert_eq!(engine.get(&out), 0);

    // 第二次 get：在 stabilize 期间会触发 width.set(1)，并要求同一次 get 内收敛到 1
    trigger.set(true);
    assert_eq!(engine.get(&out), 1);
}

#[test]
fn peek_value_respects_ready() {
    let mut engine = Engine::new();
    let (anchor_watch, anchor_setter) = {
        let var = crate::expert::Var::new(1i32);
        (var.watch(), var)
    };
    let derived = anchor_watch.map(|v| v + 1);

    // 首次 get 驱动计算，随后可直接 peek。
    assert_eq!(engine.get(&derived), 2);
    assert_eq!(engine.peek_value(&derived), Some(2));

    // set 后：在 stabilize 之前 peek 必须返回 None（避免读到过期缓存）。
    anchor_setter.set(5);
    assert!(engine.peek_value(&derived).is_none());

    // stabilize 后再次读取，peek 与 get 保持一致。
    assert_eq!(engine.get(&derived), 6);
    assert_eq!(engine.peek_value(&derived), Some(6));
}

#[test]
fn is_token_alive_reflects_drop_and_remove() {
    ////////////////////////////////////////////////////////////////////////////////
    // NOTE:
    // - `AnchorToken(NodeKey)` 是弱引用：handle_count 归零后会触发 free/remove；
    // - 上层（emg_element）会用 `engine.is_token_alive(token)` 过滤 backfill/patch 中的 stale token；
    // - 该测试用于锁定 `is_token_alive` 的语义：drop 后应变为 false。
    ////////////////////////////////////////////////////////////////////////////////
    let engine = Engine::new();

    let var = Var::new(1i32);
    let watch = var.watch();
    let token = watch.token();

    // 节点仍被 handle 持有时应为 alive。
    assert!(
        engine.is_token_alive(token),
        "节点仍被 handle 持有时应为 alive"
    );

    // drop 掉最后一个 handle（Var 自身 + watch 克隆）后，token 应当失效（anchor 置空/进入 free list）。
    drop(watch);
    drop(var);
    assert!(
        !engine.is_token_alive(token),
        "handle_count 归零并 free/remove 后应为 not alive"
    );
}

#[test]
fn test_cutoff_simple_observed() {
    let mut engine = Engine::new();
    let (v, v_setter) = {
        let var = crate::expert::Var::new(100i32);
        (var.watch(), var)
    };
    let mut old_val = 0i32;
    let post_cutoff = v
        .cutoff(move |new_val| {
            if (old_val - *new_val).abs() < 50 {
                false
            } else {
                old_val = *new_val;
                true
            }
        })
        .map(|v| *v + 10);
    engine.mark_observed(&post_cutoff);
    assert_eq!(engine.get(&post_cutoff), 110);
    v_setter.set(125);
    assert_eq!(engine.get(&post_cutoff), 110);
    v_setter.set(151);
    assert_eq!(engine.get(&post_cutoff), 161);
    v_setter.set(125);
    assert_eq!(engine.get(&post_cutoff), 161);
}

#[test]
fn test_cutoff_simple_unobserved() {
    let mut engine = Engine::new();
    let (v, v_setter) = {
        let var = crate::expert::Var::new(100i32);
        (var.watch(), var)
    };
    let mut old_val = 0i32;
    let post_cutoff = v
        .cutoff(move |new_val| {
            if (old_val - *new_val).abs() < 50 {
                false
            } else {
                old_val = *new_val;
                true
            }
        })
        .map(|v| *v + 10);
    assert_eq!(engine.get(&post_cutoff), 110);
    v_setter.set(125);
    assert_eq!(engine.get(&post_cutoff), 110);
    v_setter.set(151);
    assert_eq!(engine.get(&post_cutoff), 161);
    v_setter.set(125);
    assert_eq!(engine.get(&post_cutoff), 161);
}

#[test]
fn test_refmap_simple() {
    #[derive(PartialEq, Debug)]
    struct NoClone(usize);

    let mut engine = Engine::new();
    let (v, _) = {
        let var = crate::expert::Var::new((NoClone(1), NoClone(2)));
        (var.watch(), var)
    };
    let a = v.refmap(|(a, _)| a);
    let b = v.refmap(|(_, b)| b);
    let a_correct = a.map(|a| a == &NoClone(1));
    let b_correct = b.map(|b| b == &NoClone(2));
    assert!(engine.get(&a_correct));
    assert!(engine.get(&b_correct));
}

#[test]
fn test_split_simple() {
    let mut engine = Engine::new();
    let (v, _) = {
        let var = crate::expert::Var::new((1usize, 2usize, 3usize));
        (var.watch(), var)
    };
    let (a, b, c) = v.split();
    assert_eq!(engine.get(&a), 1);
    assert_eq!(engine.get(&b), 2);
    assert_eq!(engine.get(&c), 3);
}

#[test]
fn split_should_use_map_not_refmap() {
    ////////////////////////////////////////////////////////////////////////////////
    // NOTE:
    // - split 过去基于 refmap 做字段投影：输出借用自输入；
    // - 在 token 失效/GC 场景里，refmap 的 output() 需要 ctx.get()，容易触发 panic；
    // - 该测试用于锁定行为：split 必须走 map + clone（owned 缓存），以便安全降级。
    ////////////////////////////////////////////////////////////////////////////////
    let mut engine = Engine::new();
    let (v, _) = {
        let var = crate::expert::Var::new((1usize, 2usize));
        (var.watch(), var)
    };
    let (a, b) = v.split();

    // 先驱动一次计算，确保节点都已挂载并可导出依赖图。
    assert_eq!(engine.get(&a), 1);
    assert_eq!(engine.get(&b), 2);

    // 导出的 DOT 标签里会包含 AnchorInner 的 debug name。
    // 这里我们只关心“不能出现 refmap”，避免未来回归到不安全实现。
    let dot = engine.export_dot_from_tokens(&[a.token()]);
    assert!(
        dot.contains("map"),
        "split 派生节点应当包含 map 调试名，dot={dot}"
    );
    assert!(
        !dot.contains("refmap"),
        "split 派生节点不应再包含 refmap，dot={dot}"
    );
}

#[test]
fn test_map_simple() {
    let mut engine = Engine::new();
    let (v1, _v1_setter) = {
        let var = crate::expert::Var::new(1usize);
        (var.watch(), var)
    };
    let (v2, _v2_setter) = {
        let var = crate::expert::Var::new(123usize);
        (var.watch(), var)
    };
    let _a2 = v1.map(|num1| {
        println!("a: adding to {:?}", num1);
        *num1
    });
    let a = MultiAnchor::map((&v1, &v2), |num1, num2| num1 + num2);

    let b = MultiAnchor::map((&v1, &a, &v2), |num1, num2, num3| num1 + num2 + num3);
    engine.mark_observed(&b);
    engine.stabilize();
    assert_eq!(engine.get(&b), 248);
}

#[test]
fn test_then_simple() {
    let mut engine = Engine::new();
    let (v1, v1_setter) = {
        let var = crate::expert::Var::new(true);
        (var.watch(), var)
    };
    let (v2, _v2_setter) = {
        let var = crate::expert::Var::new(10usize);
        (var.watch(), var)
    };
    let (v3, _v3_setter) = {
        let var = crate::expert::Var::new(20usize);
        (var.watch(), var)
    };
    let a = v1.then(move |val| if *val { v2.clone() } else { v3.clone() });
    engine.mark_observed(&a);
    engine.stabilize();
    assert_eq!(engine.get(&a), 10);

    v1_setter.set(false);
    engine.stabilize();
    assert_eq!(engine.get(&a), 20);
}

#[test]
fn test_observed_marking() {
    use crate::singlethread::ObservedState;

    let mut engine = Engine::new();
    let (v1, _v1_setter) = {
        let var = crate::expert::Var::new(1usize);
        (var.watch(), var)
    };
    let a = v1.map(|num1| *num1 + 1);
    let b = a.map(|num1| *num1 + 2);
    let c = b.map(|num1| *num1 + 3);
    engine.mark_observed(&a);
    engine.mark_observed(&c);

    assert_eq!(ObservedState::Unnecessary, engine.check_observed(&v1));
    assert_eq!(ObservedState::Observed, engine.check_observed(&a));
    assert_eq!(ObservedState::Unnecessary, engine.check_observed(&b));
    assert_eq!(ObservedState::Observed, engine.check_observed(&c));

    engine.stabilize();

    assert_eq!(ObservedState::Necessary, engine.check_observed(&v1));
    assert_eq!(ObservedState::Observed, engine.check_observed(&a));
    assert_eq!(ObservedState::Necessary, engine.check_observed(&b));
    assert_eq!(ObservedState::Observed, engine.check_observed(&c));

    engine.mark_unobserved(&c);

    assert_eq!(ObservedState::Necessary, engine.check_observed(&v1));
    assert_eq!(ObservedState::Observed, engine.check_observed(&a));
    assert_eq!(ObservedState::Unnecessary, engine.check_observed(&b));
    assert_eq!(ObservedState::Unnecessary, engine.check_observed(&c));

    engine.mark_unobserved(&a);

    assert_eq!(ObservedState::Unnecessary, engine.check_observed(&v1));
    assert_eq!(ObservedState::Unnecessary, engine.check_observed(&a));
    assert_eq!(ObservedState::Unnecessary, engine.check_observed(&b));
    assert_eq!(ObservedState::Unnecessary, engine.check_observed(&c));
}

#[test]
fn test_garbage_collection_wont_panic() {
    let mut engine = Engine::new();
    let (v1, _v1_setter) = {
        let var = crate::expert::Var::new(1usize);
        (var.watch(), var)
    };
    engine.get(&v1);
    std::mem::drop(v1);
    engine.stabilize();
}

#[test]
fn test_readme_example() {
    // example
    use crate::singlethread::{Engine, MultiAnchor, Var};
    let mut engine = Engine::new();

    // create a couple `Var`s
    let (my_name, my_name_updater) = {
        let var = Var::new("Bob".to_string());
        (var.watch(), var)
    };
    let (my_unread, my_unread_updater) = {
        let var = Var::new(999usize);
        (var.watch(), var)
    };

    // `my_name` is a `Var`, our first type of `Anchor`. we can pull an `Anchor`'s value out with our `engine`:
    assert_eq!(&engine.get(&my_name), "Bob");
    assert_eq!(engine.get(&my_unread), 999);

    // we can create a new `Anchor` from another one using `map`. The function won't actually run until absolutely necessary.
    // also feel free to clone an `Anchor` — the clones will all refer to the same inner state
    let my_greeting = my_name.clone().map(|name| {
        println!("calculating name!");
        format!("Hello, {}!", name)
    });
    assert_eq!(engine.get(&my_greeting), "Hello, Bob!"); // prints "calculating name!"

    // we can update a `Var` with its updater. values are cached unless one of its dependencies changes
    assert_eq!(engine.get(&my_greeting), "Hello, Bob!"); // doesn't print anything
    my_name_updater.set("Robo".to_string());
    assert_eq!(engine.get(&my_greeting), "Hello, Robo!"); // prints "calculating name!"

    // a `map` can take values from multiple `Anchor`s. just use tuples:
    let header = (&my_greeting, &my_unread)
        .map(|greeting, unread| format!("{} You have {} new messages.", greeting, unread));
    assert_eq!(
        engine.get(&header),
        "Hello, Robo! You have 999 new messages."
    );

    // just like a future, you can dynamically decide which `Anchor` to use with `then`:
    let (insulting_name, _) = {
        let var = Var::new("Lazybum".to_string());
        (var.watch(), var)
    };
    let dynamic_name = my_unread.then(move |unread| {
        // only use the user's real name if the have less than 100 messages in their inbox
        if *unread < 100 {
            my_name.clone()
        } else {
            insulting_name.clone()
        }
    });
    assert_eq!(engine.get(&dynamic_name), "Lazybum");
    my_unread_updater.set(50);
    assert_eq!(engine.get(&dynamic_name), "Robo");
}
