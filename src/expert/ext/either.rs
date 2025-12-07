use tracing::debug;

/*
 * @Author: Rais
 * @Date: 2023-03-23 12:55:22
 * @LastEditTime: 2023-03-27 16:35:46
 * @LastEditors: Rais
 * @Description:
 */
use crate::expert::{
    Anchor, AnchorHandle, AnchorInner, Engine, OutputContext, Poll, UpdateContext, ValOrAnchor,
};
use std::panic::Location;

pub struct Either<A, Out, F, E: Engine> {
    pub(super) f: F,
    pub(super) either_anchor: Option<ValOrAnchor<Out, E>>,
    pub(super) output_stale: bool,
    pub(super) anchors: A,
    pub(super) location: &'static Location<'static>,
}

macro_rules! impl_tuple_either {
    ($([$output_type:ident, $num:tt])+) => {
        impl<$($output_type,)+ E, F, Out> AnchorInner<E> for
        Either<( $(Anchor<$output_type, E>,)+ ), Out, F, E>
        where
            F: for<'any> FnMut($(&'any $output_type),+) -> ValOrAnchor<Out, E>,
            Out: PartialEq + 'static,
            $(
                $output_type: 'static,
            )+
            E: Engine,
        {
            type Output = Out;

            fn dirty(&mut self, edge: &<E::AnchorHandle as AnchorHandle>::Token) {
                if self.output_stale {
                    return;
                }
                $(
                    // only invalidate either_anchor if one of the lhs anchors is invalidated
                    if edge == &self.anchors.$num.data.token() {
                        self.output_stale = true;
                        return;
                    }
                )+

            }
            fn poll_updated<G: UpdateContext<Engine=E>>(
                &mut self,
                ctx: &mut G,
            ) -> Poll {
                let mut poll = Poll::Unchanged;

                if self.either_anchor.is_none() || self.output_stale {
                    let mut found_pending = false;
                    let mut found_updated = false;

                    $(
                        match ctx.request(&self.anchors.$num, true) {
                            Poll::Pending | Poll::PendingDefer => {
                                found_pending = true;
                            }
                            Poll::Updated => {
                                found_updated = true;
                            }
                            Poll::Unchanged => {
                                // do nothing
                            }
                        }
                    )+

                    if found_pending {
                        return Poll::Pending;
                    }

                    self.output_stale = false;

                    if self.either_anchor.is_none() || found_updated {

                        let new_either_anchor = (self.f)($(ctx.get(&self.anchors.$num)),+);

                        match (&self.either_anchor, &new_either_anchor) {
                            (None, ValOrAnchor::Anchor(_)) => {
                                // poll = Poll::Updated;
                            }
                            (None, ValOrAnchor::Val(_)) => {
                                poll = Poll::Updated;
                            }

                            (Some(ValOrAnchor::Val(_)), ValOrAnchor::Anchor(_)) => {
                                poll = Poll::Updated;
                            }
                            (Some(ValOrAnchor::Val(a)), ValOrAnchor::Val(b)) => {
                                if a != b {
                                    poll = Poll::Updated;
                                }
                            }
                            (Some(ValOrAnchor::Anchor(outdated_anchor)), ValOrAnchor::Val(_)) => {
                                debug!("either anchor - val");

                                ctx.unrequest(outdated_anchor);
                                poll = Poll::Updated;
                            }

                            (
                                Some(ValOrAnchor::Anchor(outdated_anchor)),
                                ValOrAnchor::Anchor(new_anchor),
                            ) => {
                                debug!("either anchor - anchor");

                                if outdated_anchor != new_anchor {
                                    ctx.unrequest(outdated_anchor);
                                    poll = Poll::Updated;
                                }
                            }
                        }
                        self.either_anchor = Some(new_either_anchor);
                    }
                }

                match self.either_anchor.as_ref().unwrap() {
                    ValOrAnchor::Val(_) => poll,
                    ValOrAnchor::Anchor(an) => match ctx.request(an, true) {
                        Poll::Updated => Poll::Updated,
                        Poll::Unchanged => poll,
                        Poll::Pending | Poll::PendingDefer => Poll::Pending,
                    },
                }
            }
            fn output<'slf, 'out, G: OutputContext<'out, Engine=E>>(
                &'slf self,
                ctx: &mut G,
            ) -> &'out Self::Output
            where
                'slf: 'out,
            {
                match self.either_anchor.as_ref().unwrap() {
                    ValOrAnchor::Val(v) => v,
                    ValOrAnchor::Anchor(an) => ctx.get(an),
                }
            }

            fn debug_location(&self) -> Option<(&'static str, &'static Location<'static>)> {
                Some(("either", self.location))
            }
        }
    }
}

impl_tuple_either! {
    [O0, 0]
}

impl_tuple_either! {
    [O0, 0]
    [O1, 1]
}
impl_tuple_either! {
    [O0, 0]
    [O1, 1]
    [O2, 2]
}

impl_tuple_either! {
    [O0, 0]
    [O1, 1]
    [O2, 2]
    [O3, 3]
}

impl_tuple_either! {
    [O0, 0]
    [O1, 1]
    [O2, 2]
    [O3, 3]
    [O4, 4]
}

impl_tuple_either! {
    [O0, 0]
    [O1, 1]
    [O2, 2]
    [O3, 3]
    [O4, 4]
    [O5, 5]
}

impl_tuple_either! {
    [O0, 0]
    [O1, 1]
    [O2, 2]
    [O3, 3]
    [O4, 4]
    [O5, 5]
    [O6, 6]
}

impl_tuple_either! {
    [O0, 0]
    [O1, 1]
    [O2, 2]
    [O3, 3]
    [O4, 4]
    [O5, 5]
    [O6, 6]
    [O7, 7]
}

impl_tuple_either! {
    [O0, 0]
    [O1, 1]
    [O2, 2]
    [O3, 3]
    [O4, 4]
    [O5, 5]
    [O6, 6]
    [O7, 7]
    [O8, 8]
}

#[cfg(test)]
mod tests {
    use tracing::debug;

    use crate::{
        expert::MultiAnchor,
        singlethread::{Engine, Var, VarVOA},
    };

    #[test]
    fn either_tuple() {
        let mut engine = Engine::new();

        let a = VarVOA::new(1);
        let b = Var::new(2);
        let c = Var::new(3);
        let d = Var::new(4);

        let c2 = c.clone();
        let d2 = d.clone();

        let a2 = a.clone();
        let b2 = b.clone();

        let aw = (&a.watch(), &b.watch()).either(move |aa, bb| {
            if aa + bb < 3 {
                c2.watch().into()
            } else if aa + bb == 3 {
                100.into()
            } else {
                d2.watch().into()
            }
        });

        let awthen = aw.then(move |&x| if x > 3 { a2.watch() } else { b2.watch() });
        let awmap = aw.map(move |&x| x + 1);
        assert_eq!(engine.get(&aw), 100);
        assert_eq!(engine.get(&awthen), 1);
        assert_eq!(engine.get(&awmap), 101);

        a.set(2);
        assert_eq!(engine.get(&aw), 4);
        assert_eq!(engine.get(&awthen), 2);
        assert_eq!(engine.get(&awmap), 5);

        b.set(0);
        assert_eq!(engine.get(&aw), 3);
        assert_eq!(engine.get(&awthen), 0);
        assert_eq!(engine.get(&awmap), 4);

        c.set(10);
        assert_eq!(engine.get(&aw), 10);
        assert_eq!(engine.get(&awthen), 2);
        assert_eq!(engine.get(&awmap), 11);
    }

    #[test]
    fn aaaxx2() {
        let mut engine = Engine::new();
        let a = VarVOA::new(1);
        let c = Var::new(22);
        let d = Var::new(-1);

        let b = Var::new(9);
        let bw = b.watch();
        let bw2 = bw.clone();

        let w = a.watch().either(move |&x| {
            debug!("in either  ,  x:{x}");
            if x >= 2 { bw.clone().into() } else { x.into() }
        });
        let ww = w.map(|x| {
            debug!("in map  ww,  x:{x}");
            x + 1
        });

        assert_eq!(engine.get(&w), 1);
        assert_eq!(engine.get(&ww), 2);
        a.set(-1);
        assert_eq!(engine.get(&w), -1);
        assert_eq!(engine.get(&ww), 0);
        a.set(3);
        assert_eq!(engine.get(&w), 9);
        assert_eq!(engine.get(&ww), 10);
        b.set(11);
        assert_eq!(engine.get(&w), 11);
        assert_eq!(engine.get(&ww), 12);
        a.set(-2);
        assert_eq!(engine.get(&w), -2);
        assert_eq!(engine.get(&ww), -1);

        debug!(".....................");
        a.set(c.watch());
        // a.set(bw2);
        debug!("a seted .....................");

        // debug!("a:{}", engine.get(a.get().as_anchor().unwrap()));
        // debug!("a:{}", engine.get(&a.watch()));
        debug!("w:{}", engine.get(&w));
        assert_eq!(engine.get(&w), 11);
        assert_eq!(engine.get(&ww), 12);

        a.set(d.watch());
        assert_eq!(engine.get(&w), -1);
        assert_eq!(engine.get(&ww), 0);

        a.set(c.watch());
        assert_eq!(engine.get(&w), 11);
        assert_eq!(engine.get(&ww), 12);
    }

    #[test]
    fn aaaxx() {
        let mut engine = Engine::new();
        let a = VarVOA::new(1);
        let c = Var::new(22);
        let d = Var::new(-1);
        let b = Var::new(9);
        let bw = b.watch();
        let bw2 = bw.clone();

        let w = a.watch().either(move |&x| {
            debug!("in either  ,  x:{x}");
            if x >= 2 { bw.clone().into() } else { x.into() }
        });

        assert_eq!(engine.get(&w), 1);
        a.set(-1);
        assert_eq!(engine.get(&w), -1);
        a.set(3);
        assert_eq!(engine.get(&w), 9);
        b.set(11);
        assert_eq!(engine.get(&w), 11);
        a.set(-2);
        assert_eq!(engine.get(&w), -2);
        debug!(".....................");
        // a.set(c.watch());
        a.set(bw2.clone());
        debug!("a seted .....................");

        // debug!("a:{}", engine.get(a.get().as_anchor().unwrap()));
        // debug!("a:{}", engine.get(&a.watch()));
        debug!("w:{}", engine.get(&w));
        assert_eq!(engine.get(&w), 11);

        a.set(d.watch());
        debug!("w:{}", engine.get(&w));
        assert_eq!(engine.get(&w), -1);

        a.set(bw2);
        debug!("a seted .....................");

        debug!("w:{}", engine.get(&w));
        assert_eq!(engine.get(&w), 11);
    }
}
