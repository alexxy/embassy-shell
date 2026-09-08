use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use crate::Result;
use crate::io::{BoxFuture, Io};

/// Arguments of a command invocation: the whitespace-split, quote-unescaped
/// tokens of the line, excluding the command name itself.
#[derive(Clone, Copy)]
pub struct Args<'a> {
    parts: &'a [String],
}

impl<'a> Args<'a> {
    pub(crate) fn new(parts: &'a [String]) -> Self {
        Args { parts }
    }

    /// Number of arguments.
    pub fn len(&self) -> usize {
        self.parts.len()
    }

    /// Whether there are no arguments.
    pub fn is_empty(&self) -> bool {
        self.parts.is_empty()
    }

    /// Argument at index `i`, if present.
    pub fn get(&self, i: usize) -> Option<&str> {
        self.parts.get(i).map(|s| s.as_str())
    }

    /// Iterate over the arguments.
    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.parts.iter().map(|s| s.as_str())
    }

    /// The arguments joined back with single spaces (useful for commands like
    /// `echo` that consume the rest of the line verbatim).
    pub fn rest(&self) -> String {
        let mut out = String::new();
        for (i, part) in self.parts.iter().enumerate() {
            if i > 0 {
                out.push(' ');
            }
            out.push_str(part);
        }
        out
    }
}

impl fmt::Debug for Args<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.parts).finish()
    }
}

/// Signature of a command handler. See [`Shell::add_command`](crate::Shell::add_command).
pub(crate) type Handler<'a> =
    Box<dyn for<'x> Fn(Args<'x>, Io<'x>) -> BoxFuture<'x, Result<()>> + 'a>;

/// Signature of a per-command tab completer: called with the index of the
/// argument being completed (`>= 1`) and the current partial token, returns a
/// list of candidate replacements for that token.
pub(crate) type Completer<'a> = Box<dyn Fn(usize, &str) -> Vec<String> + 'a>;

/// How a command completes its arguments. Fixed option lists are stored
/// directly (no boxed closure, no heap candidates), keeping completion of
/// `add_command_with_options` commands allocation-free.
pub(crate) enum CompleterKind<'a> {
    Options(&'static [&'static str]),
    Custom(Completer<'a>),
}

pub(crate) struct Command<'a> {
    pub name: String,
    pub help: &'static str,
    pub handler: Handler<'a>,
    pub completer: Option<CompleterKind<'a>>,
}
