use core::fmt;

use embedded_io::ErrorKind;

/// Error type used by the shell and returned by command handlers.
///
/// Transport errors are wrapped into [`Error::Io`] so that every command
/// handler can use the same concrete error type regardless of the underlying
/// transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// An I/O error occurred on the underlying transport.
    Io(ErrorKind),
    /// An operation failed for another reason.
    Other,
}

impl core::error::Error for Error {}

impl embedded_io::Error for Error {
    fn kind(&self) -> ErrorKind {
        match self {
            Error::Io(kind) => *kind,
            Error::Other => ErrorKind::Other,
        }
    }
}

impl From<ErrorKind> for Error {
    fn from(kind: ErrorKind) -> Self {
        Error::Io(kind)
    }
}

#[cfg(feature = "defmt")]
impl defmt::Format for Error {
    fn format(&self, f: defmt::Formatter<'_>) {
        match self {
            // `embedded_io::ErrorKind` does not implement `defmt::Format`,
            // so the kind is not carried on the wire.
            Error::Io(_) => defmt::write!(f, "io error"),
            Error::Other => defmt::write!(f, "shell error"),
        }
    }
}

impl Error {
    /// Short, allocation-free description used by the shell's error line.
    ///
    /// Unlike [`Display`](core::fmt::Display) this returns a `&'static str`
    /// so the shell never has to run `core::fmt` (or allocate a buffer) just
    /// to report a failed command.
    pub fn message(&self) -> &'static str {
        match self {
            Error::Other => "shell error",
            Error::Io(kind) => match *kind {
                ErrorKind::NotFound => "not found",
                ErrorKind::PermissionDenied => "permission denied",
                ErrorKind::ConnectionRefused => "connection refused",
                ErrorKind::ConnectionReset => "connection reset",
                ErrorKind::ConnectionAborted => "connection aborted",
                ErrorKind::NotConnected => "not connected",
                ErrorKind::AddrInUse => "address in use",
                ErrorKind::AddrNotAvailable => "address not available",
                ErrorKind::BrokenPipe => "broken pipe",
                ErrorKind::AlreadyExists => "already exists",
                ErrorKind::InvalidInput => "invalid input",
                ErrorKind::InvalidData => "invalid data",
                ErrorKind::TimedOut => "timed out",
                ErrorKind::Interrupted => "interrupted",
                ErrorKind::Unsupported => "unsupported",
                ErrorKind::OutOfMemory => "out of memory",
                ErrorKind::WriteZero => "write zero",
                _ => "io error",
            },
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(kind) => write!(f, "io error: {kind:?}"),
            Error::Other => write!(f, "shell error"),
        }
    }
}

/// Convenience alias used throughout the crate and by command handlers.
pub type Result<T> = core::result::Result<T, Error>;
