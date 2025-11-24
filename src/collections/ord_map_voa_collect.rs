/*
 * @Author: Rais
 * @Date: 2023-04-03 14:30:01
 * @LastEditTime: 2023-04-05 00:12:41
 * @LastEditors: Rais
 * @Description:
 */

use im_rc::ordmap;

use crate::{
    collections::ord_map_collect::OrdMapCollect,
    expert::{ValOrAnchor, constant::Constant},
    im::OrdMap,
};

use crate::expert::Anchor;
use crate::expert::Engine;
// ─────────────────────────────────────────────────────────────────────────────

impl<I, V, E> From<OrdMap<I, ValOrAnchor<V, E>>> for Anchor<OrdMap<I, V>, E>
where
    <E as Engine>::AnchorHandle: PartialOrd + Ord,
    V: std::clone::Clone + 'static + std::cmp::PartialEq,
    I: 'static + Clone + std::cmp::Ord,
    E: Engine,
    // OrdMap<I, V>: std::cmp::Eq,
{
    fn from(value: OrdMap<I, ValOrAnchor<V, E>>) -> Self {
        OrdMapVOACollect::new_to_anchor(value)
    }
}
impl<I, V, E> From<Anchor<OrdMap<I, ValOrAnchor<V, E>>, E>> for Anchor<OrdMap<I, V>, E>
where
    <E as Engine>::AnchorHandle: PartialOrd + Ord,
    V: std::clone::Clone + 'static + std::cmp::PartialEq,
    I: 'static + Clone + std::cmp::Ord,
    E: Engine,
    // OrdMap<I, V>: std::cmp::Eq,
{
    fn from(value: Anchor<OrdMap<I, ValOrAnchor<V, E>>, E>) -> Self {
        value.then(|v| OrdMapVOACollect::new_to_anchor(v.clone()))
    }
}

impl<I, V, E> std::iter::FromIterator<(I, ValOrAnchor<V, E>)> for Anchor<OrdMap<I, V>, E>
where
    <E as Engine>::AnchorHandle: PartialOrd + Ord,
    V: std::clone::Clone + 'static + std::cmp::PartialEq,
    I: 'static + Clone + std::cmp::Ord,
    E: Engine,
    // OrdMap<I, V>: std::cmp::Eq,
{
    fn from_iter<T>(iter: T) -> Self
    where
        T: IntoIterator<Item = (I, ValOrAnchor<V, E>)>,
    {
        OrdMapVOACollect::new_to_anchor(iter.into_iter().collect())
    }
}

impl<'a, I, V, E> std::iter::FromIterator<&'a (I, ValOrAnchor<V, E>)> for Anchor<OrdMap<I, V>, E>
where
    <E as Engine>::AnchorHandle: PartialOrd + Ord,
    V: std::clone::Clone + 'static + std::cmp::PartialEq,
    I: 'static + Clone + std::cmp::Ord,
    E: Engine,
    // OrdMap<I, V>: std::cmp::Eq,
{
    fn from_iter<T>(iter: T) -> Self
    where
        T: IntoIterator<Item = &'a (I, ValOrAnchor<V, E>)>,
    {
        OrdMapVOACollect::new_to_anchor(iter.into_iter().cloned().collect())
    }
}

impl<'a, I, V, E> std::iter::FromIterator<(&'a I, &'a ValOrAnchor<V, E>)>
    for Anchor<OrdMap<I, V>, E>
where
    <E as Engine>::AnchorHandle: PartialOrd + Ord,
    V: std::clone::Clone + 'static + std::cmp::PartialEq,
    I: 'static + Clone + std::cmp::Ord,
    E: Engine,
    // OrdMap<I, V>: std::cmp::Eq,
{
    fn from_iter<T>(iter: T) -> Self
    where
        T: IntoIterator<Item = (&'a I, &'a ValOrAnchor<V, E>)>,
    {
        OrdMapVOACollect::new_to_anchor(
            iter.into_iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        )
    }
}

/// 过渡器：将 `ValOrAnchor` 预先“固化”为稳定的 Anchor，再复用已有的 `OrdMapCollect`。
/// 好处：依赖图在构造时就成型，避免在 `poll_updated` 过程中临时变更依赖链引发重入。
pub struct OrdMapVOACollect;

impl OrdMapVOACollect {
    /// 将 `OrdMap<I, ValOrAnchor<V>>` 映射为 `OrdMap<I, Anchor<V>>`，然后直接交给 `OrdMapCollect`
    /// 处理，保证依赖在构造期就稳定下来。
    #[track_caller]
    pub fn new_to_anchor<I, V, E>(anchors: OrdMap<I, ValOrAnchor<V, E>>) -> Anchor<OrdMap<I, V>, E>
    where
        <E as Engine>::AnchorHandle: PartialOrd + Ord,
        V: Clone + 'static + std::cmp::PartialEq,
        I: 'static + Clone + std::cmp::Ord,
        E: Engine,
    {
        let anchor_map = Self::voa_dict_to_anchor_dict(anchors);
        OrdMapCollect::new_to_anchor(anchor_map)
    }

    /// 把 VOA 字典转换为“稳定 Anchor”字典：`Val` 立即包成 Constant，`Anchor` 原样传递。
    fn voa_dict_to_anchor_dict<I, V, E>(
        anchors: OrdMap<I, ValOrAnchor<V, E>>,
    ) -> OrdMap<I, Anchor<V, E>>
    where
        V: Clone + 'static + std::cmp::PartialEq,
        I: Clone + std::cmp::Ord,
        E: Engine,
    {
        let pool = ordmap::OrdMapPool::new(anchors.len());
        let mut dict = OrdMap::with_pool(&pool);

        anchors
            .into_iter()
            .map(|(i, voa)| {
                let anchor = match voa {
                    ValOrAnchor::Val(v) => Constant::new_internal::<E>(v),
                    ValOrAnchor::Anchor(an) => an,
                };
                (i, anchor)
            })
            .collect_into(&mut dict);
        dict
    }
}

#[cfg(test)]
mod test {

    use crate::{collections::ord_map_methods::Dict, im::OrdMap};

    use crate::{dict, singlethread::*};

    #[test]
    fn collect_k_change() {
        let mut engine = Engine::new();
        let a = Var::new(1);
        let av = 1;
        let b = Var::new(2);
        let bv = 2;
        let c = Var::new(5);
        let cv = 5;
        let d = Var::new(10);
        let dv = 10;

        // ─────────────────────────────────────────────────────────────

        let dict: Dict<usize, ValOrAnchor<usize>> = dict!(
            1usize=>a.watch().into(),
            2usize=>av.into(),
            3usize=>b.watch().into(),
            4usize=>bv.into(),
            5usize=>c.watch().into(),
            6usize=>cv.into(),
            7usize=>d.watch().into(),
            8usize=>dv.into()
        );
        let f = Var::new(dict.clone());

        let nums = f.watch().then(|dict| {
            // let nums: Anchor<OrdMap<_, _>> = d.into_iter().collect();
            let nums: Anchor<OrdMap<usize, usize>> = dict.clone().into();
            nums
        });
        let sum: Anchor<usize> = nums.map(|nums| nums.values().sum());
        assert_eq!(engine.get(&sum), 36);
        f.set(dict!(
            9usize=>a.watch().into(),
            2usize=>av.into(),
            3usize=>b.watch().into(),
            4usize=>bv.into(),
            5usize=>c.watch().into(),
            6usize=>cv.into(),
            7usize=>d.watch().into(),
            8usize=>dv.into()
        ));
        assert_eq!(engine.get(&sum), 36);
        f.set(dict!(

            1usize=>bv.into(),
            4usize=>c.watch().into(),


            7usize=>dv.into()
        ));
        assert_eq!(engine.get(&sum), 17);

        // ─────────────────────────────────────────────────────────────

        let f2 = Var::new(dict);

        let watch = f2.watch();
        let nums2: Anchor<OrdMap<usize, usize>> = watch.into();
        let sum2: Anchor<usize> = nums2.map(|nums| nums.values().sum());
        assert_eq!(engine.get(&sum2), 36);
        f2.set(dict!(
            9usize=>a.watch().into(),
            2usize=>av.into(),
            3usize=>b.watch().into(),
            4usize=>bv.into(),
            5usize=>c.watch().into(),
            6usize=>cv.into(),
            7usize=>d.watch().into(),
            8usize=>dv.into()
        ));
        assert_eq!(engine.get(&sum2), 36);
        f2.set(dict!(

            1usize=>bv.into(),
            4usize=>c.watch().into(),


            7usize=>dv.into()
        ));
        assert_eq!(engine.get(&sum2), 17);
    }

    #[test]
    fn collect() {
        let mut engine = Engine::new();

        let a = Var::new(1);
        let av = 1;
        let b = Var::new(2);
        let bv = 2;
        let c = Var::new(5);
        let cv = 5;
        let d = Var::new(10);
        let dv = 10;

        let dict: Dict<usize, ValOrAnchor<usize>> = dict!(
            1usize=>a.watch().into(),
            2usize=>av.into(),
            3usize=>b.watch().into(),
            4usize=>bv.into(),
            5usize=>c.watch().into(),
            6usize=>cv.into(),
            7usize=>d.watch().into(),
            8usize=>dv.into()
        );
        let f = Var::new(dict.clone());

        let nums: Anchor<OrdMap<usize, usize>> = f.watch().into();
        let sum: Anchor<usize> = nums.map(|nums| nums.values().sum());

        assert_eq!(engine.get(&sum), 36);

        a.set(2);
        assert_eq!(engine.get(&sum), 37);

        f.set(dict!(
            1usize=>a.watch().into(),
            2usize=>10.into(),
            3usize=>b.watch().into(),
            4usize=>bv.into(),
            5usize=>c.watch().into(),
            6usize=>cv.into(),
            7usize=>d.watch().into(),
            8usize=>dv.into()
        ));
        assert_eq!(engine.get(&sum), 37 + 9);
    }
}
