#![feature(negative_impls)]
#![feature(exact_size_is_empty)]
#![feature(specialization)]
#![feature(option_result_contains)] //for with_anchor func
                                    // ─────────────────────────────────────────────────────────────────────────────

pub mod collections;
pub mod expert;
pub mod singlethread;

// ────────────────────────────────────────────────────────────────────────────────

// pub use crate::im as im;
pub use imbl as im;
#[cfg(test)]
#[allow(unused_variables)]
mod tests {
    use crate::{
        dict,
        singlethread::{Engine, Var},
    };

    #[test]
    fn dict_test() {
        let engine = Engine::new();
        let a = Var::new(1);
        let b = Var::new(2);
        let c = Var::new(5);
        let d = Var::new(10);
        // let mut xx = dict!(1usize=>1,2usize=>1,3usize=>1);
        let mut xx1 = dict!(0=>1,2=>1,9=>1);
        xx1.insert(4, 2);
        println!("{xx1:?}");

        // let f = Var::new(dict!(1usize=>a.watch(),2usize=>b.watch(),3usize=>c.watch()));
        // let nums = f.watch().then(|d| {
        //     let nums: Anchor<OrdMap<_, _>> = d.into_iter().collect();
        //     nums
        // });
        // let sum: Anchor<usize> = nums.map(|nums| nums.values().sum());
        // assert_eq!(engine.get(&sum), 8);
        // f.set(dict!(9usize=>a.watch(),2usize=>b.watch(),3usize=>c.watch()));
    }
}

// #[macro_use]
// use im::ordmap::OrdMap;
#[macro_export]
macro_rules! dict {
    () => { $crate::im::ordmap::OrdMap::new() };

    ( $( $key:expr => $value:expr ),* ) => {{
        let mut map = $crate::im::ordmap::OrdMap::new();
        $({
            map.insert($key, $value);
        })*;
        map
    }};
}
#[macro_export]
macro_rules! dict_k_into {
    () => { $crate::im::ordmap::OrdMap::new() };

    ( $( $key:expr => $value:expr ),* ) => {{
        let mut map = $crate::im::ordmap::OrdMap::new();
        $({
            map.insert($key .into(), $value);
        })*;
        map
    }};
}

#[cfg(test)]
mod test {
    #[test]
    fn test() {
        let _cc = dict! {1=>1};
    }
}
