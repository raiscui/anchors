use tracing::error;

use crate::expert::{
    Anchor, AnchorHandle, AnchorInner, Engine, OutputContext, Poll, UpdateContext,
};
use std::{any::Any, panic::Location};

/// An Anchor type for immutable values.
pub struct Constant<T> {
    val: T,
    first_poll: bool,
    location: &'static Location<'static>,
}

impl<T: 'static> Constant<T> {
    /// Creates a new Constant Anchor from some value.
    // #[track_caller]
    // #[deprecated]
    // pub fn new<E: Engine>(val: T) -> Anchor<T, E> {
    //     Self::new_internal(val)
    // }

    #[track_caller]
    pub(crate) fn new_internal<E: Engine>(val: T) -> Anchor<T, E> {
        E::mount(Self {
            val,
            first_poll: true,
            location: Location::caller(),
        })
    }

    #[cfg(test)]
    pub fn new_raw_testing(val: T) -> Constant<T> {
        Self {
            val,
            first_poll: true,
            location: Location::caller(),
        }
    }
}

impl<T: 'static, E: Engine> AnchorInner<E> for Constant<T> {
    type Output = T;
    fn dirty(&mut self, edge: &<E::AnchorHandle as AnchorHandle>::Token) {
        let e = edge as &dyn Any;
        let nodekey = e
            .downcast_ref::<crate::singlethread::AnchorToken>()
            .unwrap();
        let ng = unsafe { nodekey.ptr.lookup_unchecked() };

        error!(
            target:"anchors",
            "Constant never has any inputs; dirty should not have been called. alleged child: {:?},child info: {:?}  ,type: {:?},\nloc{:?}",
            edge,
            ng.debug_info.get(),
            std::any::type_name::<T>(),
            self.location
        )
    }
    fn poll_updated<G: UpdateContext<Engine = E>>(&mut self, _ctx: &mut G) -> Poll {
        let res = if self.first_poll {
            Poll::Updated
        } else {
            Poll::Unchanged
        };
        self.first_poll = false;
        res
    }
    fn output<'slf, 'out, G: OutputContext<'out, Engine = E>>(
        &'slf self,
        _ctx: &mut G,
    ) -> &'out Self::Output
    where
        'slf: 'out,
    {
        &self.val
    }

    fn debug_location(&self) -> Option<(&'static str, &'static Location<'static>)> {
        Some(("constant", self.location))
    }
}
