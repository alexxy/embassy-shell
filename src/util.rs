use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

/// Minimal `futures::poll_fn` replacement so the crate has no dependency on
/// `futures-util`.
pub(crate) struct PollFn<F> {
    f: F,
}

impl<F> Unpin for PollFn<F> {}

impl<F, T> Future for PollFn<F>
where
    F: FnMut(&mut Context<'_>) -> Poll<T>,
{
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        (self.get_mut().f)(cx)
    }
}

pub(crate) fn poll_fn<F, T>(f: F) -> PollFn<F>
where
    F: FnMut(&mut Context<'_>) -> Poll<T>,
{
    PollFn { f }
}
