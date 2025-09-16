/*
 * @Author: Rais
 * @Date: 2023-01-20 17:00:31
 * @LastEditTime: 2023-03-17 14:32:56
 * @LastEditors: Rais
 * @Description:
 */
use crate::im::{OrdSet, ordset::DiffItem};

use crate::expert::{Anchor, Engine};

impl<E: Engine, K: Clone + Ord + 'static> Anchor<OrdSet<K>, E> {
    #[track_caller]
    pub fn filter<F: FnMut(&K) -> bool + 'static>(&self, f: F) -> Anchor<OrdSet<K>, E> {
        self.filter_map(f)
    }

    // TODO rlord: fix this name god

    // #[track_caller]

    // pub fn map_<F: FnMut(&K) -> T + 'static, T: Clone + PartialEq + 'static>(
    //     &self,
    //     mut f: F,
    // ) -> Anchor<OrdSet<K>, E> {
    //     self.filter_map(move |k| Some(f(k)))
    // }

    /// FOOBAR
    #[track_caller]
    pub fn filter_map<F: FnMut(&K) -> bool + 'static>(&self, mut f: F) -> Anchor<OrdSet<K>, E> {
        self.unordered_fold(OrdSet::new(), move |out, diff_item, len| {
            let out_is_empty = out.is_empty();
            if len == 0 && !out_is_empty {
                out.clear();
                return true;
            } else if len == 0 && out_is_empty {
                return false;
            }

            match diff_item {
                DiffItem::Add(k) => {
                    if f(k) {
                        out.insert(k.clone());
                        return true;
                    }
                }
                DiffItem::Update { old: o, new: k } => {
                    //TODO  if key change , v not?
                    #[cfg(debug_assertions)]
                    {
                        if o != k {
                            panic!("key mismatch");
                        }
                    }

                    if f(k) {
                        out.insert(k.clone());
                        return true;
                    // } else if out.contains_key(k) {
                    } else if out.remove(o).is_some() {
                        return true;
                    }
                }
                DiffItem::Remove(k) => {
                    if out.remove(k).is_some() {
                        return true;
                    };
                }
            }
            false
        })
    }

    /// Dict 增加/更新 K V 会增量执行 function f , 用于更新 out,
    /// Dict 移除 K V 并不会触发 out 的更新,
    #[track_caller]
    pub fn increment_reduction<F, T>(&self, initial_state: T, mut f: F) -> Anchor<T, E>
    where
        F: FnMut(&mut T, &K) + 'static,
        T: Clone + PartialEq + 'static,
    {
        self.unordered_fold(initial_state, move |out, diff_item, len| {
            if len == 0 {
                return false;
            }
            match diff_item {
                DiffItem::Add(k) => {
                    f(out, k);

                    true
                }
                DiffItem::Update { old: _, new: k } => {
                    //TODO  if key change , v not?
                    f(out, k);
                    true
                }
                DiffItem::Remove(_k) => false,
            }
        })
    }

    /// Dict 增加/更新 K V 会增量执行 function f , 用于更新 out,
    /// Dict 移除 K V 也会执行 remove f,
    #[track_caller]
    pub fn reduction<F, Fu, T>(
        &self,
        initial_state: T,
        mut add: F,
        mut update: Fu,
        mut remove: F,
    ) -> Anchor<T, E>
    where
        F: FnMut(&mut T, &K) -> bool + 'static,
        Fu: FnMut(&mut T, &K, &K) -> bool + 'static,
        T: Clone + PartialEq + 'static + std::iter::ExactSizeIterator,
    {
        self.unordered_fold(initial_state, move |out, diff_item, _len| {
            // ─────────────────────────────────────────────────────

            match diff_item {
                DiffItem::Add(k) => add(out, k),
                DiffItem::Update { old: o, new: k } => update(out, o, k),
                DiffItem::Remove(k) => remove(out, k),
            }
        })
    }

    #[track_caller]
    pub fn unordered_fold<
        T: PartialEq + Clone + 'static,
        F: for<'a> FnMut(&mut T, DiffItem<'a, K>, usize) -> bool + 'static,
        // F: for<'a, 'b> FnMut(&mut T, DiffItem<'a, 'b, K>, usize) -> bool + 'static,
    >(
        &self,
        initial_state: T,
        mut f: F,
    ) -> Anchor<T, E> {
        cfg_if::cfg_if! {
            if #[cfg(feature = "pool")]{
                let mut last_observation = Dict::new();
                let mut initd = false;
                self.map_mut(initial_state, move |out, this| {
                    if !initd {
                        last_observation = Dict::with_pool(this.pool());
                        initd = true;
                    }

                    let mut did_update = false;
                    let len = this.len();
                    // if last_observation.len() == 0 && len==0 {
                    //     return false
                    // }

                    // if len == 0 && last_observation.len() != 0{
                    //     out.clear();
                    //     last_observation.clear();
                    //     return true;
                    // }

                    for item in last_observation.diff(this) {
                        if f(out, item, len) {
                            did_update = true;
                        }
                    }
                    last_observation = this.clone();
                    did_update
                })
            }else{

                let mut last_observation = OrdSet::new();
                self.map_mut(initial_state, move |out, this| {


                    let mut did_update = false;
                    let len = this.len();
                    // if last_observation.len() == 0 && len==0 {
                    //     return false
                    // }

                    // if len == 0 && last_observation.len() != 0{
                    //     out.clear();
                    //     last_observation.clear();
                    //     return true;
                    // }

                    for item in last_observation.diff(this) {
                        if f(out, item, len) {
                            did_update = true;
                        }
                    }
                    last_observation = this.clone();
                    did_update
                })

            }

        }
    }
}

#[cfg(test)]
mod test {

    use crate::collections::ord_map_methods::Dict;

    use super::*;
    #[test]
    fn test_filter() {
        let mut engine = crate::singlethread::Engine::new();
        let mut ordset = OrdSet::new();
        let a = crate::expert::Var::new(ordset.clone());
        let b = a.watch().filter(|n| n == "b" || n == "d");
        let b_out = engine.get(&b);
        assert_eq!(0, b_out.len());

        ordset.insert("a".to_string());
        ordset.insert("b".to_string());
        ordset.insert("c".to_string());
        ordset.insert("d".to_string());
        a.set(ordset.clone());
        let b_out = engine.get(&b);
        assert_eq!(2, b_out.len());

        ordset.insert("a".to_string());
        ordset.insert("b".to_string());
        ordset.remove("d");
        ordset.insert("e".to_string());
        a.set(ordset.clone());
        let b_out = engine.get(&b);
        assert_eq!(1, b_out.len());
    }

    #[test]
    fn test_map() {
        let mut engine = crate::singlethread::Engine::new();
        let mut dict = Dict::new();
        let a = crate::expert::Var::new(dict.clone());
        let b = a.watch().map_(1, |_, n| *n + 1);
        let b_out = engine.get(&b);
        assert_eq!(0, b_out.len());

        dict.insert("a".to_string(), 1);
        dict.insert("b".to_string(), 2);
        dict.insert("c".to_string(), 3);
        dict.insert("d".to_string(), 4);
        a.set(dict.clone());
        let b_out = engine.get(&b);
        assert_eq!(4, b_out.len());
        assert_eq!(Some(&2), b_out.get("a"));
        assert_eq!(Some(&3), b_out.get("b"));
        assert_eq!(Some(&4), b_out.get("c"));
        assert_eq!(Some(&5), b_out.get("d"));

        dict.insert("a".to_string(), 10);
        dict.insert("b".to_string(), 11);
        dict.remove("d");
        dict.insert("e".to_string(), 12);
        a.set(dict.clone());
        let b_out = engine.get(&b);
        assert_eq!(4, b_out.len());
        assert_eq!(Some(&11), b_out.get("a"));
        assert_eq!(Some(&12), b_out.get("b"));
        assert_eq!(Some(&4), b_out.get("c"));
        assert_eq!(Some(&13), b_out.get("e"));
    }
}
