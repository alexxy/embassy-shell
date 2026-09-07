//! A tiny `no_std` interactive shell in the spirit of bash, designed to run
//! on top of [embassy](https://www.embassy.dev) (or any async executor) over
//! any transport that implements [`embedded_io_async::Read`] /
//! [`embedded_io_async::Write`] — UART, USB CDC, RTT-with-serial-gwakeup, ...
//!
//! # Features
//!
//! * Bash-like line editing: cursor keys, Home/End, Delete, Backspace,
//!   Ctrl-U (kill line), Ctrl-W (delete word), Ctrl-L (clear screen).
//! * `Tab` completion of command names and per-command argument completion
//!   (fixed option lists or custom completer callbacks), with bash-style
//!   common-prefix insertion and candidate listing.
//! * Command history (Up/Down), configurable size.
//! * Ctrl-C interrupts a *running* command (its handler future is dropped).
//! * Simple command registration with closures; commands get an output handle
//!   ([`Io`]) so they can `await` slow writes (e.g. to USB CDC).
//! * Built-in `help`, `history`, `clear` (overridable by user commands).
//! * No dependency on `futures`/`tokio`; only `embedded-io`/
//!   `embedded-io-async`. Uses `alloc` (a global allocator is required).
//!
//! # Example
//!
//! ```
//! use embassy_shell::Shell;
//!
//! let mut shell = Shell::new();
//!
//! shell.add_command("hello", "greet someone", |args, mut io| {
//!     Box::pin(async move {
//!         let name = args.get(0).unwrap_or("world");
//!         io.println(&format!("Hello, {name}!")).await
//!     })
//! });
//!
//! shell.add_command_with_options(
//!     "led",
//!     "switch the led on or off",
//!     &["on", "off"],
//!     |args, mut io| Box::pin(async move {
//!         io.println(match args.get(0) {
//!             Some("on") => "led on",
//!             Some("off") => "led off",
//!             _ => "usage: led on|off",
//!         })
//!         .await
//!     }),
//! );
//!
//! let mut input: &[u8] = b"hello bob\nled on\n";
//! let mut output: Vec<u8> = Vec::new();
//! futures::executor::block_on(shell.run(&mut input, &mut output)).unwrap();
//!
//! let text = String::from_utf8_lossy(&output);
//! assert!(text.contains("Hello, bob!"));
//! assert!(text.contains("led on"));
//! ```
//!
//! # Using it with embassy
//!
//! With embassy, run the shell as a task over a UART or a USB CDC serial
//! port. Both implement the required traits:
//!
//! ```ignore
//! // `uart` is an embassy-stm32/esp/nrf UART handle, or a USB CDC
//! // `SerialPort` from embassy-usb — anything implementing
//! // embedded-io-async Read + Write.
//! #[embassy_executor::task]
//! async fn shell_task(mut uart: Uart<'static, PERIPHERALS>) {
//!     let mut shell = Shell::new();
//!     shell.add_command("reboot", "reset the board", |_args, mut io| {
//!         Box::pin(async move {
//!             io.println("bye!").await?;
//!             io.flush().await?;
//!             cortex_m::peripheral::SCB::sys_reset();
//!         })
//!     });
//!     shell.run(&mut uart, &mut uart).await.ok();
//! }
//! ```
//!
//! # Cargo features
//!
//! * `unicode` *(enabled by default)* — decode multi-byte UTF-8 sequences
//!   typed at the prompt. Disabling it removes the UTF-8 continuation
//!   reader from the input path and saves flash; bytes `>= 0x80` are then
//!   treated as individual Latin-1 characters, so real UTF-8 input (e.g.
//!   pasted non-ASCII text) will be garbled on display. Command handling
//!   itself is byte-oriented and unaffected.
//! * `defmt` *(disabled by default)* — emit trace-level logs (received
//!   keys, dispatched commands, completion counts) via
//!   [`defmt`](https://docs.rs/defmt). Enable with
//!   `embassy-shell = { version = "...", features = ["defmt"] }`.
//!
//! # Notes and limitations
//!
//! * Command handlers return [`BoxFuture`] (wrap an `async move` block in
//!   `Box::pin`). This type-erases per-command future types so they can live
//!   in one table. The boxed futures are not `Send`; this matches embassy's
//!   executor model (tasks are pinned to one core).
//! * Ctrl-C cancels the handler future; handlers should be cancel-safe (do
//!   not hold locks across await points, or clean up in a guard).
//! * Bytes typed while a command is running are not echoed; only the last
//!   byte typed before/after the interrupt is retained for the next line.
//! * Line editing assumes an ANSI/VT100-compatible terminal (putty, minicom,
//!   `picocom`, modern Windows Terminal, ...).

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

/// Internal trace logging: compiles to `defmt::trace!` with the `defmt`
/// feature, to nothing otherwise.
#[cfg(feature = "defmt")]
macro_rules! log {
    ($s:literal $(, $arg:expr)*) => {
        defmt::trace!($s $(, $arg)*)
    };
}

#[cfg(not(feature = "defmt"))]
macro_rules! log {
    ($($tt:tt)*) => {};
}

mod command;
mod error;
mod io;
mod keys;
mod parse;
mod shell;
mod util;

pub use command::Args;
pub use error::{Error, Result};
pub use io::{BoxFuture, Io};
pub use shell::Shell;
