#![feature(negative_impls)]

pub mod collections;
pub mod expert;
pub mod singlethread;

// ────────────────────────────────────────────────────────────────────────────────

pub use im_rc;

// #[macro_use]
// use im::ordmap::OrdMap;
#[macro_export]
macro_rules! dict {
    () => { $crate::im_rc::ordmap::OrdMap::new() };

    ( $( $key:expr => $value:expr ),* ) => {{
        let mut map = $crate::im_rc::ordmap::OrdMap::new();
        $({
            map.insert($key, $value);
        })*;
        map
    }};
}

#[cfg(test)]
mod test {
    #[test]
    fn test() {
        let cc = dict! {1=>1};
    }
}
