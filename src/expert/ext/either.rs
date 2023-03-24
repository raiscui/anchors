use tracing::debug;

/*
 * @Author: Rais
 * @Date: 2023-03-23 12:55:22
 * @LastEditTime: 2023-03-24 14:56:40
 * @LastEditors: Rais
 * @Description:
 */
use crate::expert::{
    Anchor, AnchorHandle, AnchorInner, EitherAnchor, Engine, OutputContext, Poll, UpdateContext,
};
use std::panic::Location;

pub struct Either<A, Out, F, E: Engine> {
    pub(super) f: F,
    pub(super) either_anchor: Option<EitherAnchor<Out, E>>,
    pub(super) output_stale: bool,
    pub(super) anchors: A,
    pub(super) location: &'static Location<'static>,
}

impl<O0, E, F, Out> AnchorInner<E> for Either<(Anchor<O0, E>,), Out, F, E>
where
    F: for<'any> FnMut(&'any O0) -> EitherAnchor<Out, E>,
    Out: PartialEq + 'static + std::fmt::Debug,
    O0: 'static,
    E: Engine,
{
    type Output = Out;
    fn dirty(&mut self, edge: &<E::AnchorHandle as AnchorHandle>::Token) {
        // return self.output_stale = true;
        debug!("either in dirty");
        if edge == &self.anchors.0.data.token() {
            debug!("either ---is dirty");

            self.output_stale = true;
            return;
        }
    }
    fn poll_updated<G: UpdateContext<Engine = E>>(&mut self, ctx: &mut G) -> Poll {
        let mut poll = Poll::Unchanged;
        if self.either_anchor.is_none() || self.output_stale {
            debug!("either changed");

            let mut found_pending = false;
            let mut found_updated = false;
            match ctx.request(&self.anchors.0, true) {
                Poll::Pending => {
                    debug!("either parent pending");

                    found_pending = true;
                }
                Poll::Updated => {
                    debug!("either parent updated ");

                    found_updated = true;
                }
                Poll::Unchanged => {
                    debug!("either parent Unchanged");
                }
            }
            if found_pending {
                return Poll::Pending;
            }
            self.output_stale = false;
            if self.either_anchor.is_none() || found_updated {
                debug!("either find update");

                let new_either_anchor = (self.f)(ctx.get(&self.anchors.0));
                match (&self.either_anchor, &new_either_anchor) {
                    (None, EitherAnchor::Anchor(_)) => {
                        // poll = Poll::Updated;
                    }
                    (None, EitherAnchor::Val(_)) => {
                        poll = Poll::Updated;
                    }

                    (Some(EitherAnchor::Val(_)), EitherAnchor::Anchor(_)) => {
                        poll = Poll::Updated;
                    }
                    (Some(EitherAnchor::Val(a)), EitherAnchor::Val(b)) => {
                        if a != b {
                            poll = Poll::Updated;
                        }
                    }
                    (Some(EitherAnchor::Anchor(outdated_anchor)), EitherAnchor::Val(_)) => {
                        debug!("either anchor - val");

                        ctx.unrequest(outdated_anchor);
                        poll = Poll::Updated;
                    }

                    (
                        Some(EitherAnchor::Anchor(outdated_anchor)),
                        EitherAnchor::Anchor(new_anchor),
                    ) => {
                        debug!("either anchor - anchor");

                        if outdated_anchor != new_anchor {
                            ctx.unrequest(outdated_anchor);
                            poll = Poll::Updated;
                        }
                    }
                }
                debug!("either reset either_anchor");

                self.either_anchor = Some(new_either_anchor);
            }
        }

        match self.either_anchor.as_ref().unwrap() {
            EitherAnchor::Val(_) => poll,
            EitherAnchor::Anchor(an) => match ctx.request(an, true) {
                Poll::Updated => Poll::Updated,
                Poll::Unchanged => poll,
                Poll::Pending => Poll::Pending,
            },
        }
    }
    fn output<'slf, 'out, G: OutputContext<'out, Engine = E>>(
        &'slf self,
        ctx: &mut G,
    ) -> &'out Self::Output
    where
        'slf: 'out,
    {
        match self.either_anchor.as_ref().unwrap() {
            EitherAnchor::Val(v) => {
                debug!("either output:{v:?} ");

                v
            }
            EitherAnchor::Anchor(an) => ctx.get(an),
        }
    }
    fn debug_location(&self) -> Option<(&'static str, &'static Location<'static>)> {
        Some(("Either", self.location))
    }
}

#[cfg(test)]
mod tests {
    use tracing::debug;

    use crate::singlethread::{Engine, Var, VarEA};

    #[test]
    fn aaaxx2() {
        let mut engine = Engine::new();
        let a = VarEA::new(1);
        let c = Var::new(22);
        let d = Var::new(-1);

        let b = Var::new(9);
        let bw = b.watch();
        let bw2 = bw.clone();

        let w = a.watch().either(move |&x| {
            debug!("in either  ,  x:{x}");
            if x >= 2 {
                bw.clone().into()
            } else {
                x.into()
            }
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
        let a = VarEA::new(1);
        let c = Var::new(22);
        let d = Var::new(-1);
        let b = Var::new(9);
        let bw = b.watch();
        let bw2 = bw.clone();

        let w = a.watch().either(move |&x| {
            debug!("in either  ,  x:{x}");
            if x >= 2 {
                bw.clone().into()
            } else {
                x.into()
            }
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
