use std::rc::Rc;

use anchors::singlethread::*;

fn main() {
    let mut engine = Engine::new();

    let v1 = Var::new(1);
    let v2 = v1.clone();
    v1.set(2);
    println!("v1 {:?}", &v1.get());
    println!("v2 {:?}", &v2.get());
    v2.set(3);
    println!("v1 {:?}", &v1.get());
    println!("v2 {:?}", &v2.get());
    // create a couple `Var`s
    let (my_name, my_name_updater) = {
        let var = Var::new("Bob".to_string());
        (var.watch(), var)
    };
    let (my_unread, my_unread_updater) = {
        let var = Var::new(999usize);
        (var.watch(), var)
    };
    let v = Var::new((0, 1));
    let mm = v.watch().clone();
    let xx = v.watch().then(move |x| {
        println!("v change");
        mm.map(|y| {
            println!("mm change");
            (y.0, y.1 + 1)
        })
    });
    println!("{:?}", engine.get(&xx));
    println!("=============================");
    v.set((3, 3));
    // println!("xx{:?}", engine.get(&xx));
    // println!("xx2{:?}", engine.get(&xx));
    let n = xx.refmap(|v| {
        println!("get n");
        &v.1
    });
    let xn = n.map(|x| x + 10);
    // println!("n {}", engine.get(&n));
    // println!("n2 {}", engine.get(&n));
    println!("xn {}", engine.get(&xn));
    println!("xn {}", engine.get(&xn));
    // // `my_name` is a `Var`, our first type of `Anchor`. we can pull an `Anchor`'s value out with our `engine`:
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
    // ────────────────────────────────────────────────────────────────────────────────
    // ────────────────────────────────────────────────────────────────────────────────
    let sss = Rc::new("Lazybum".to_string());
    let strvar = Var::new(sss.clone());
    let sw = strvar.watch().map(|s| {
        println!("!!!!!!!!!!str change {}", s);
        s.clone()
    });
    println!("==== sw:{:?}", engine.get(&sw));
    strvar.set(sss.clone());
    println!("==== sw:{:?}", engine.get(&sw));
    println!("==== sw:{:?}", engine.get(&sw));
}
