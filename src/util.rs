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

/// Fixed-size stack buffer used to assemble short byte sequences (ANSI
/// escapes, numbers) without heap allocation and without pulling in
/// `core::fmt` machinery. Overflowing writes are silently dropped.
pub(crate) struct StackWriter<const N: usize> {
    buf: [u8; N],
    len: usize,
}

impl<const N: usize> StackWriter<N> {
    pub(crate) fn new() -> Self {
        StackWriter {
            buf: [0; N],
            len: 0,
        }
    }

    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    /// Append a byte, dropping it if the buffer is full.
    pub(crate) fn push(&mut self, b: u8) {
        if self.len < N {
            self.buf[self.len] = b;
            self.len += 1;
        }
    }

    /// Append an ASCII decimal number, without pulling in `core::fmt`.
    pub(crate) fn push_usize(&mut self, mut v: usize) {
        let mut tmp = [0u8; 20];
        let mut i = tmp.len();
        loop {
            i -= 1;
            tmp[i] = b'0' + (v % 10) as u8;
            v /= 10;
            if v == 0 {
                break;
            }
        }
        for &b in &tmp[i..] {
            self.push(b);
        }
    }
}
