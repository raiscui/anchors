use im_rc::{ordmap::DiffItem, OrdMap};

use crate::expert::{Anchor, Engine};
pub type Dict<K, V> = OrdMap<K, V>;

impl<E: Engine, K: Ord + Clone + PartialEq + 'static, V: Clone + PartialEq + 'static>
    Anchor<Dict<K, V>, E>
where
    // (K, V): PartialEq,
    Dict<K, V>: PartialEq,
{
    #[track_caller]
    pub fn filter<F: FnMut(&K, &V) -> bool + 'static>(&self, mut f: F) -> Anchor<Dict<K, V>, E> {
        self.filter_map(move |k, v| if f(k, v) { Some(v.clone()) } else { None })
    }

    // TODO rlord: fix this name god

    #[track_caller]

    pub fn map_<F: FnMut(&K, &V) -> T + 'static, T: Clone + PartialEq + 'static>(
        &self,
        mut f: F,
    ) -> Anchor<Dict<K, T>, E> {
        self.filter_map(move |k, v| Some(f(k, v)))
    }

    /// FOOBAR
    #[track_caller]
    pub fn filter_map<F: FnMut(&K, &V) -> Option<T> + 'static, T: Clone + PartialEq + 'static>(
        &self,
        mut f: F,
    ) -> Anchor<Dict<K, T>, E> {
        self.unordered_fold(Dict::new(), move |out, diff_item, len| {
            let out_is_empty = out.is_empty();
            if len == 0 && !out_is_empty {
                out.clear();
                return true;
            } else if len == 0 && out_is_empty {
                return false;
            }
            // ─────────────────────────────────────────────────────

            match diff_item {
                DiffItem::Add(k, v) => {
                    if let Some(new) = f(k, v) {
                        out.insert(k.clone(), new);
                        return true;
                    }
                }
                DiffItem::Update {
                    old: (ok, ov),
                    new: (k, v),
                } => {
                    #[cfg(debug_assertions)]
                    {
                        if ok != k {
                            panic!("key mismatch");
                        }
                    }

                    //TODO  if key change , v not?
                    if let Some(new) = f(k, v) {
                        out.insert(k.clone(), new);
                        return true;
                    // } else if out.contains_key(k) {
                    } else if out.remove(ok).is_some() {
                        return true;
                    }
                }
                DiffItem::Remove(k, _v) => {
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
        F: FnMut(&mut T, &K, &V) + 'static,
        T: Clone + PartialEq + 'static,
    {
        self.unordered_fold(initial_state, move |out, diff_item, len| {
            if len == 0 {
                return false;
            }
            match diff_item {
                DiffItem::Add(k, v) => {
                    f(out, k, v);

                    true
                }
                DiffItem::Update {
                    old: _,
                    new: (k, v),
                } => {
                    //TODO  if key change , v not?
                    f(out, k, v);
                    true
                }
                DiffItem::Remove(_k, _v) => false,
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
        F: FnMut(&mut T, &K, &V) -> bool + 'static,
        Fu: FnMut(&mut T, (&K, &V), &K, &V) -> bool + 'static,
        T: Clone + PartialEq + 'static,
    {
        self.unordered_fold(initial_state, move |out, diff_item, len| {
            if len == 0 {
                return false;
            }
            match diff_item {
                DiffItem::Add(k, v) => add(out, k, v),
                DiffItem::Update { old, new: (k, v) } => update(out, old, k, v),
                DiffItem::Remove(k, v) => remove(out, k, v),
            }
        })
    }

    #[track_caller]
    pub fn unordered_fold<
        T: PartialEq + Clone + 'static,
        F: for<'a> FnMut(&mut T, DiffItem<'a, K, V>, usize) -> bool + 'static,
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

                let mut last_observation = Dict::new();
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

    use super::*;
    #[test]
    fn test_filter() {
        let mut engine = crate::singlethread::Engine::new();
        let mut dict = Dict::new();
        let a = crate::expert::Var::new(dict.clone());
        let b = a.watch().filter(|_, n| *n > 10);
        let b_out = engine.get(&b);
        assert_eq!(0, b_out.len());

        dict.insert("a".to_string(), 1);
        dict.insert("b".to_string(), 23);
        dict.insert("c".to_string(), 5);
        dict.insert("d".to_string(), 24);
        a.set(dict.clone());
        let b_out = engine.get(&b);
        assert_eq!(2, b_out.len());
        assert_eq!(Some(&23), b_out.get("b"));
        assert_eq!(Some(&24), b_out.get("d"));

        dict.insert("a".to_string(), 25);
        dict.insert("b".to_string(), 5);
        dict.remove("d");
        dict.insert("e".to_string(), 50);
        a.set(dict.clone());
        let b_out = engine.get(&b);
        assert_eq!(2, b_out.len());
        assert_eq!(Some(&25), b_out.get("a"));
        assert_eq!(Some(&50), b_out.get("e"));
    }

    #[test]
    fn test_map() {
        let mut engine = crate::singlethread::Engine::new();
        let mut dict = Dict::new();
        let a = crate::expert::Var::new(dict.clone());
        let b = a.watch().map_(|_, n| *n + 1);
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
