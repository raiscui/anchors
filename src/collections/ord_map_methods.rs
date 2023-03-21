use tracing::debug;

use crate::{
    expert::MultiAnchor,
    im::{ordmap::DiffItem, OrdMap},
};

use crate::expert::{Anchor, Engine};
pub type Dict<K, V> = OrdMap<K, V>;

impl<E: Engine, K: Ord + Clone + PartialEq + 'static, V: Clone + PartialEq + 'static>
    Anchor<Dict<K, V>, E>
where
    // (K, V): PartialEq,
    Dict<K, V>: PartialEq,
{
    #[track_caller]
    pub fn filter<F: FnMut(&K, &V) -> bool + 'static>(
        &self,
        pool_size: usize,
        mut f: F,
    ) -> Anchor<Dict<K, V>, E> {
        self.filter_map(
            pool_size,
            move |k, v| if f(k, v) { Some(v.clone()) } else { None },
        )
    }
    #[track_caller]
    pub fn filter_with_anchor<A, F>(
        &self,
        pool_size: usize,
        anchor: &Anchor<A, E>,
        mut f: F,
    ) -> Anchor<Dict<K, V>, E>
    where
        A: 'static + std::cmp::PartialEq + std::clone::Clone,
        F: FnMut(&A, &K, &V) -> bool + 'static,
    {
        self.filter_map_with_anchor(pool_size, anchor, move |a, k, v| {
            if f(a, k, v) {
                Some(v.clone())
            } else {
                None
            }
        })
    }

    // TODO rlord: fix this name god

    #[track_caller]
    pub fn map_<F: FnMut(&K, &V) -> T + 'static, T: Clone + PartialEq + 'static>(
        &self,
        pool_size: usize,
        mut f: F,
    ) -> Anchor<Dict<K, T>, E> {
        self.filter_map(pool_size, move |k, v| Some(f(k, v)))
    }
    #[track_caller]
    pub fn map_with_anchor<
        A: 'static + std::cmp::PartialEq + std::clone::Clone,
        F: FnMut(&A, &K, &V) -> T + 'static,
        T: Clone + PartialEq + 'static,
    >(
        &self,
        pool_size: usize,
        anchor: &Anchor<A, E>,
        mut f: F,
    ) -> Anchor<Dict<K, T>, E> {
        self.filter_map_with_anchor(pool_size, anchor, move |a, k, v| Some(f(a, k, v)))
    }

    /// FOOBAR
    #[track_caller]
    pub fn filter_map<F: FnMut(&K, &V) -> Option<T> + 'static, T: Clone + PartialEq + 'static>(
        &self,
        pool_size: usize,
        mut f: F,
    ) -> Anchor<Dict<K, T>, E> {
        let pool = crate::im::ordmap::OrdMapPool::new(pool_size);
        let dict = OrdMap::with_pool(&pool);

        self.unordered_fold(dict, move |out, diff_item, len| {
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
                    old: (ok, _ov),
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
    #[track_caller]
    pub fn filter_map_with_anchor<
        A: 'static + std::cmp::PartialEq + Clone,
        F: FnMut(&A, &K, &V) -> Option<T> + 'static,
        T: Clone + PartialEq + 'static,
    >(
        &self,
        pool_size: usize,
        anchor: &Anchor<A, E>,
        mut f: F,
    ) -> Anchor<Dict<K, T>, E> {
        let pool = crate::im::ordmap::OrdMapPool::new(pool_size);
        let dict = OrdMap::with_pool(&pool);

        self.unordered_fold_with_anchor(anchor, dict, move |a, out, diff_item, len| {
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
                    if let Some(new) = f(a, k, v) {
                        out.insert(k.clone(), new);
                        return true;
                    }
                }
                DiffItem::Update {
                    old: (ok, _ov),
                    new: (k, v),
                } => {
                    #[cfg(debug_assertions)]
                    {
                        if ok != k {
                            panic!("key mismatch");
                        }
                    }

                    if let Some(new) = f(a, k, v) {
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
        // F: for<'a, 'b> FnMut(&mut T, DiffItem<'a, 'b, K, V>, usize) -> bool + 'static,
    >(
        &self,
        initial_state: T,
        mut f: F,
    ) -> Anchor<T, E> {
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

    ///如果 anchor 更改, 则全部元素执行func
    pub fn unordered_fold_with_anchor<
        A: 'static + std::cmp::PartialEq + Clone,
        T: PartialEq + Clone + 'static,
        // F: for<'a, 'b> FnMut(&A, &mut T, DiffItem<'a, 'b, K, V>, usize) -> bool + 'static,
        F: for<'a, 'b> FnMut(&A, &mut T, DiffItem<'a, K, V>, usize) -> bool + 'static,
    >(
        &self,
        anchor: &Anchor<A, E>,
        initial_state: T,
        mut f: F,
    ) -> Anchor<T, E> {
        let mut last_observation = Dict::new();
        let mut last_observation_a = None;
        let initial_state_save = initial_state.clone();

        (anchor, self).map_mut(initial_state, move |out, a, this| {
            // debug!("a {:?}",a);
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

            if !last_observation_a.contains(a) {
                last_observation.clear();
                *out = initial_state_save.clone();
            }

            for item in last_observation.diff(this) {
                if f(a, out, item, len) {
                    debug!("did Updated");
                    did_update = true;
                }
            }
            last_observation = this.clone();
            last_observation_a = Some(a.clone());

            did_update
        })
    }
}

#[cfg(test)]
mod test {

    // use imbl::ordmap;
    use crate::im::ordmap;

    use crate::singlethread::Var;

    use super::*;
    #[test]
    fn with_anchor() {
        let mut engine = crate::singlethread::Engine::new();
        let mut dict = Dict::new();
        let a = Var::new(dict.clone());

        dict.insert("a".to_string(), 1);
        dict.insert("b".to_string(), 23);
        dict.insert("c".to_string(), 5);
        dict.insert("d".to_string(), 24);
        a.set(dict.clone());

        let var_1 = Var::new("b");
        let anchor1 = var_1.watch();
        let filtered = a.watch().filter_map_with_anchor(10, &anchor1, |aa, k, v| {
            debug!("k={} a={}", k, aa);
            if k != aa {
                Some(*v)
            } else {
                None
            }
        });

        let expected = ordmap! {"a".to_string()=>1, "c".to_string()=>5,"d".to_string()=>24};
        assert_eq!(engine.get(&filtered), expected);

        var_1.set("c");
        let expected1 = ordmap! {"a".to_string()=>1, "b".to_string()=>23,"d".to_string()=>24};

        assert_eq!(engine.get(&filtered), expected1);

        let sum = filtered.map(|d| d.values().sum::<i32>());
        assert_eq!(engine.get(&sum), 1 + 23 + 24);

        // ─────────────────────────────────────────────────────────────────────────────

        let mut dict = Dict::<String, i32>::new();
        let a = Var::new(dict.clone());
        let var_1 = Var::new("b");
        let anchor1 = var_1.watch();
        let filtered = a.watch().filter_map_with_anchor(8, &anchor1, |aa, k, v| {
            debug!("k={} a={}", k, aa);
            if k != aa {
                Some(*v)
            } else {
                None
            }
        });

        let sum = filtered.map(|d| d.values().sum::<i32>());
        assert_eq!(engine.get(&sum), 0);

        dict.insert("a".to_string(), 1);
        a.set(dict.clone());
        assert_eq!(engine.get(&sum), 1);

        dict.insert("a".to_string(), 2);
        a.set(dict.clone());
        assert_eq!(engine.get(&sum), 2);

        dict.remove("a");
        a.set(dict.clone());
        assert_eq!(engine.get(&sum), 0);

        dict.insert("b".to_string(), 3);
        a.set(dict.clone());
        assert_eq!(engine.get(&sum), 0);

        var_1.set("c");
        assert_eq!(engine.get(&sum), 3);

        dict.insert("c".to_string(), 4);
        a.set(dict.clone());
        assert_eq!(engine.get(&sum), 3);

        var_1.set("d");
        assert_eq!(engine.get(&sum), 7);
    }

    #[test]
    fn test_filter() {
        let mut engine = crate::singlethread::Engine::new();
        let mut dict = Dict::new();
        let a = crate::expert::Var::new(dict.clone());
        let b = a.watch().filter(0, |_, n| *n > 10);
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
        let b = a.watch().map_(4, |_, n| *n + 1);
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
