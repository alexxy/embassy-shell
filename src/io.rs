use alloc::boxed::Box;
use core::future::Future;
use core::pin::Pin;

use embedded_io::Error as _;

use crate::error::{Error, Result};

/// A boxed future used as the return type of command handlers.
///
/// Command handlers return this type so that every command shares one
/// concrete (type-erased) future type, which is what makes it possible to
/// store arbitrary commands in a single table.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

/// Dyn-compatible writer abstraction used to type-erase the concrete
/// transport writer from command handlers.
trait DynWrite {
    fn dyn_write_all<'a>(&'a mut self, buf: &'a [u8]) -> BoxFuture<'a, Result<()>>;
    fn dyn_flush<'a>(&'a mut self) -> BoxFuture<'a, Result<()>>;
}

impl<W> DynWrite for W
where
    W: embedded_io_async::Write + ?Sized,
{
    fn dyn_write_all<'a>(&'a mut self, buf: &'a [u8]) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            embedded_io_async::Write::write_all(self, buf)
                .await
                .map_err(|e| Error::Io(e.kind()))
        })
    }

    fn dyn_flush<'a>(&'a mut self) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            embedded_io_async::Write::flush(self)
                .await
                .map_err(|e| Error::Io(e.kind()))
        })
    }
}

/// Output handle passed to command handlers.
///
/// Wraps the shell's writer. The concrete transport type is hidden, so a
/// command works unchanged over UART, USB CDC, etc.
///
/// [`Io`] also implements [`embedded_io_async::Write`], so commands can pass
/// it to any code expecting that trait (error type [`Error`]).
pub struct Io<'a> {
    w: &'a mut dyn DynWrite,
}

impl<'a> Io<'a> {
    pub(crate) fn new<W: embedded_io_async::Write>(w: &'a mut W) -> Self {
        Io { w }
    }

    /// Write a string slice, flushing so that it reaches the terminal.
    pub async fn print(&mut self, s: &str) -> Result<()> {
        self.w.dyn_write_all(s.as_bytes()).await?;
        self.w.dyn_flush().await
    }

    /// Write a string slice followed by `\r\n`, flushing the transport.
    pub async fn println(&mut self, s: &str) -> Result<()> {
        self.w.dyn_write_all(s.as_bytes()).await?;
        self.w.dyn_write_all(b"\r\n").await?;
        self.w.dyn_flush().await
    }

    /// Write a byte slice, flushing the transport.
    pub async fn write_all(&mut self, buf: &[u8]) -> Result<()> {
        self.w.dyn_write_all(buf).await?;
        self.w.dyn_flush().await
    }

    /// Flush the underlying writer.
    pub async fn flush(&mut self) -> Result<()> {
        self.w.dyn_flush().await
    }
}

impl embedded_io::ErrorType for Io<'_> {
    type Error = Error;
}

impl embedded_io_async::Write for Io<'_> {
    async fn write(&mut self, buf: &[u8]) -> core::result::Result<usize, Error> {
        self.w.dyn_write_all(buf).await?;
        Ok(buf.len())
    }

    async fn flush(&mut self) -> core::result::Result<(), Error> {
        self.w.dyn_flush().await
    }
}
