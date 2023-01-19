/*
 * @Author: Rais
 * @Date: 2022-05-19 11:16:05
 * @LastEditTime: 2022-05-19 12:54:33
 * @LastEditors: Rais
 * @Description:
 */
use std::rc::Rc;

use anchors::singlethread::*;

fn main() {
    let mut engine = Engine::new();

    let v1 = Var::new(1);
    let v1b = Var::new(1);
    v1.set(2);
    v1b.set(2);
    println!("v1 {:?}", &v1.get());
    println!("v1b {:?}", &v1b.get());
    println!("=----");
    let v2 = v1.watch().map(|x| {
        println!("*** recalculate v2 ***");
        x + 1
    });
    let v2b = v1b.watch().map(|x| x + 1);
    println!("=---------------------------------- 1次");
    println!("v2 {:?}", engine.get(&v2));
    println!("=---------------------------------- 2次");
    println!("v2 {:?}", engine.get(&v2));
    assert_eq!(engine.get(&v2), 3);
    v1.set(3);
    println!("=---------------------------------- 3次");

    assert_eq!(engine.get(&v2), 4);
}
