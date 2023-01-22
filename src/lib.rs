#![feature(negative_impls)]
#![feature(exact_size_is_empty)]
// ─────────────────────────────────────────────────────────────────────────────

pub mod collections;
pub mod expert;
pub mod singlethread;

// ────────────────────────────────────────────────────────────────────────────────

pub use im_rc as im;

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
