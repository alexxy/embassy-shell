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
