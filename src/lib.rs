#![feature(negative_impls)]
#![feature(exact_size_is_empty)]
#![feature(specialization)]
// #![feature(option_result_contains)] //for with_anchor func
#![feature(iter_collect_into)]
#![feature(auto_traits)]
#![feature(unboxed_closures)]
#![feature(tuple_trait)]
#![feature(fn_traits)]

// ─────────────────────────────────────────────────────────────────────────────

pub mod collections;
pub mod expert;
pub mod singlethread;
pub mod telemetry;

// ────────────────────────────────────────────────────────────────────────────────

// pub use imbl as im;
pub use im_rc as im;

// ─────────────────────────────────────────────────────────────────────────────

pub trait FnMutInto2<Res2, Args = ()>
where
    Self: FnMut<Args> + 'static,
    Args: std::marker::Tuple,
    Res2: From<Self::Output>,
{
    fn call_into(&mut self, args: Args) -> Res2;
}

impl<Args, Res2, Fun> FnMutInto2<Res2, Args> for Fun
where
    Self: FnMut<Args> + 'static,
    Args: std::marker::Tuple,
    Res2: From<Self::Output>,
{
    fn call_into(&mut self, args: Args) -> Res2
// where
        //     Res2: From<<Fun as FnOnce<Args>>::Output>,
    {
        (self).call_mut(args).into()
    }
}
pub trait FnMutInto<Args = ()>
where
    Self: FnMut<Args> + 'static,
    Args: std::marker::Tuple,
{
    fn call_into<Res2>(&mut self, args: Args) -> Res2
    where
        Res2: From<Self::Output>;
}

impl<Args, Fun> FnMutInto<Args> for Fun
where
    Self: FnMut<Args> + 'static,
    Args: std::marker::Tuple,
{
    fn call_into<Res2>(&mut self, args: Args) -> Res2
    where
        Res2: From<Self::Output>,
        // where
        //     Res2: From<<Fun as FnOnce<Args>>::Output>,
    {
        (self).call_mut(args).into()
    }
}

// macro_rules! impls_fnmut_into {
//     ( $( $name:ident )* ) => {
//         impl<Args,Fun, $($name),* > FnMutInto<Res2, ($(&'a $name,)*)> for Fun
//         where
//             Fun: FnMut( $(&'a $name),*) -> Res  + 'static,

//         {
//             type Res<X> = Res;

//             fn invoke<X,F2>(&mut self,  args: ($(&'a $name,)*) , mut chain:F2) -> Res2
//             where
//                 F2: FnMut(Res) -> Res2  + 'static,
//             {
//                 #[allow(non_snake_case)]
//                 let ($($name,)*) = args;
//                 chain((self)($($name,)*))
//             }
//         }
//     };
// }

// // impls_fnmut_chain! {}

// impls_fnmut_chain! { A }
// impls_fnmut_chain! { A B C }

// we support method arities up to 16
// impls_fnmut_chain! { A B C D E F G H I J K L M N O P }

#[cfg(test)]
mod xxx {
    use super::FnMutInto;

    #[test]
    fn ffff() {
        let mut fnn = |x: &i32, y: &i32| x + y + 1;
        let r: i32 = fnn.call_into((&2, &3));
        let s: i64 = 1i32.into();
        assert_eq!(r, 6);
    }
}
// ─────────────────────────────────────────────────────────────────────────────

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
    fn test() {}
}
