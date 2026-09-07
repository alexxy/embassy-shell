use embedded_io::Error as _;

use crate::error::{Error, Result};

/// A decoded key press received from the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum Key {
    /// A printable character.
    Char(char),
    /// Enter (CR or LF).
    Enter,
    /// Backspace / DEL.
    Backspace,
    /// Forward delete (`ESC [ 3 ~`).
    Delete,
    /// Tab — triggers completion.
    Tab,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    /// Ctrl-C — interrupt.
    CtrlC,
    /// Ctrl-U — kill line.
    CtrlU,
    /// Ctrl-W — delete previous word.
    CtrlW,
    /// Ctrl-L — clear screen.
    CtrlL,
}

pub(crate) async fn read_byte<R>(reader: &mut R, pending: &mut Option<u8>) -> Result<Option<u8>>
where
    R: embedded_io_async::Read,
{
    if let Some(b) = pending.take() {
        return Ok(Some(b));
    }
    let mut buf = [0u8; 1];
    match reader.read(&mut buf).await {
        Ok(1) => Ok(Some(buf[0])),
        // Ok(0) on a non-empty buffer means EOF.
        Ok(_) => Ok(None),
        Err(e) => Err(Error::Io(e.kind())),
    }
}

/// Read one key from the terminal, decoding UTF-8 and common ANSI/VT escape
/// sequences (xterm-compatible cursor keys).
///
/// Returns `Ok(None)` on end of input or for escape sequences that are
/// ignored.
pub(crate) async fn read_key<R>(reader: &mut R, pending: &mut Option<u8>) -> Result<Option<Key>>
where
    R: embedded_io_async::Read,
{
    let Some(b) = read_byte(reader, pending).await? else {
        return Ok(None);
    };

    let key = match b {
        b'\r' | b'\n' => Key::Enter,
        0x08 | 0x7F => Key::Backspace,
        b'\t' => Key::Tab,
        0x03 => Key::CtrlC,
        0x15 => Key::CtrlU,
        0x17 => Key::CtrlW,
        0x0C => Key::CtrlL,
        0x1B => {
            // ESC [ <final> — ignore anything else.
            let Some(b'[') = read_byte(reader, pending).await? else {
                return Ok(None);
            };
            let Some(b3) = read_byte(reader, pending).await? else {
                return Ok(None);
            };
            match b3 {
                b'A' => Key::Up,
                b'B' => Key::Down,
                b'C' => Key::Right,
                b'D' => Key::Left,
                b'H' => Key::Home,
                b'F' => Key::End,
                b'3' => {
                    // ESC [ 3 ~ (delete); other `ESC [ 3 <x>` are ignored.
                    match read_byte(reader, pending).await? {
                        Some(b'~') => Key::Delete,
                        _ => return Ok(None),
                    }
                }
                _ => return Ok(None),
            }
        }
        0x20..=0x7E => Key::Char(b as char),
        #[cfg(feature = "unicode")]
        0xC2..=0xDF => match read_utf8_char(reader, pending, b, 2).await? {
            Some(c) => Key::Char(c),
            None => return Ok(None),
        },
        #[cfg(feature = "unicode")]
        0xE0..=0xEF => match read_utf8_char(reader, pending, b, 3).await? {
            Some(c) => Key::Char(c),
            None => return Ok(None),
        },
        #[cfg(feature = "unicode")]
        0xF0..=0xF7 => match read_utf8_char(reader, pending, b, 4).await? {
            Some(c) => Key::Char(c),
            None => return Ok(None),
        },
        // Without the `unicode` feature, high bytes pass through as
        // individual Latin-1 characters (no UTF-8 continuation decoding).
        #[cfg(not(feature = "unicode"))]
        0x80..=0xFF => Key::Char(b as char),
        // Unmapped control character.
        _ => return Ok(None),
    };

    Ok(Some(key))
}

/// Read `len - 1` continuation bytes after `lead` and decode one UTF-8
/// character. Returns `None` on EOF or for invalid sequences.
#[cfg(feature = "unicode")]
async fn read_utf8_char<R>(
    reader: &mut R,
    pending: &mut Option<u8>,
    lead: u8,
    len: usize,
) -> Result<Option<char>>
where
    R: embedded_io_async::Read,
{
    let mut buf = [0u8; 4];
    buf[0] = lead;
    for slot in buf[1..len].iter_mut() {
        match read_byte(reader, pending).await? {
            Some(b) => *slot = b,
            None => return Ok(None),
        }
    }
    Ok(core::str::from_utf8(&buf[..len])
        .ok()
        .and_then(|s| s.chars().next()))
}
