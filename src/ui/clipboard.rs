//! System clipboard via OSC52 escape sequences.
//!
//! OSC52 is a terminal escape (`ESC ] 52 ; c ; <base64-data> BEL`) that asks
//! the terminal emulator to copy a payload into the system clipboard. It works
//! through SSH (because the data flows over the same channel as the rest of
//! the TUI), without spawning xclip/wl-copy and without bringing in a
//! cross-platform clipboard crate. Most modern terminals support it; some
//! (e.g. Apple Terminal, older GNOME Terminal) silently no-op, which is why
//! `copy` returns a bool the caller can surface in a status line.
//!
//! Limitation: many terminals cap the clipboard payload size (~75 KB in
//! tmux/wezterm/kitty). We pre-truncate at [`MAX_PAYLOAD`] so a 1 MB log line
//! doesn't get silently dropped.

#![allow(dead_code)]

use std::io::{self, Write};

/// Maximum number of *raw* bytes we attempt to copy. Larger payloads are
/// truncated with a "…[truncated]" marker because most terminals reject them.
pub const MAX_PAYLOAD: usize = 64 * 1024;

const OSC52_PREFIX: &str = "\x1b]52;c;";
const OSC52_SUFFIX: &str = "\x07";

/// Copy `text` to the system clipboard via OSC52. Returns the number of bytes
/// actually sent (post-truncation), or an `io::Error` if writing to the
/// terminal failed.
///
/// Whether the terminal honoured the request is not knowable from inside the
/// program — the caller should display a friendly status hint and let the
/// user verify by pasting.
pub fn copy(text: &str) -> io::Result<usize> {
    copy_to(io::stdout().lock(), text)
}

/// Test seam — same as [`copy`] but lets a unit test capture output.
pub fn copy_to<W: Write>(mut sink: W, text: &str) -> io::Result<usize> {
    let payload = truncate_for_clipboard(text);
    let encoded = base64_encode(payload.as_bytes());
    sink.write_all(OSC52_PREFIX.as_bytes())?;
    sink.write_all(encoded.as_bytes())?;
    sink.write_all(OSC52_SUFFIX.as_bytes())?;
    sink.flush()?;
    Ok(payload.len())
}

fn truncate_for_clipboard(text: &str) -> std::borrow::Cow<'_, str> {
    if text.len() <= MAX_PAYLOAD {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut end = MAX_PAYLOAD;
    while !text.is_char_boundary(end) && end > 0 {
        end -= 1;
    }
    let mut out = String::with_capacity(end + 16);
    out.push_str(&text[..end]);
    out.push_str("…[truncated]");
    std::borrow::Cow::Owned(out)
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    let mut iter = input.chunks_exact(3);
    for chunk in iter.by_ref() {
        let n = ((chunk[0] as u32) << 16) | ((chunk[1] as u32) << 8) | (chunk[2] as u32);
        out.push(B64[((n >> 18) & 0x3f) as usize] as char);
        out.push(B64[((n >> 12) & 0x3f) as usize] as char);
        out.push(B64[((n >> 6) & 0x3f) as usize] as char);
        out.push(B64[(n & 0x3f) as usize] as char);
    }
    let rem = iter.remainder();
    match rem.len() {
        0 => {}
        1 => {
            let n = (rem[0] as u32) << 16;
            out.push(B64[((n >> 18) & 0x3f) as usize] as char);
            out.push(B64[((n >> 12) & 0x3f) as usize] as char);
            out.push('=');
            out.push('=');
        }
        2 => {
            let n = ((rem[0] as u32) << 16) | ((rem[1] as u32) << 8);
            out.push(B64[((n >> 18) & 0x3f) as usize] as char);
            out.push(B64[((n >> 12) & 0x3f) as usize] as char);
            out.push(B64[((n >> 6) & 0x3f) as usize] as char);
            out.push('=');
        }
        _ => unreachable!(),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_known_vectors() {
        // RFC 4648 examples.
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn copy_to_writes_osc52_envelope() {
        let mut buf = Vec::new();
        let n = copy_to(&mut buf, "hello").unwrap();
        assert_eq!(n, 5);
        let s = String::from_utf8(buf).unwrap();
        assert!(s.starts_with("\x1b]52;c;"));
        assert!(s.ends_with('\x07'));
        // "hello" → "aGVsbG8="
        assert!(s.contains("aGVsbG8="));
    }

    #[test]
    fn truncates_oversized_payload_at_char_boundary() {
        // Build a string just over the cap with a multi-byte character at
        // the boundary, so the truncate path has to step back.
        let mut s = "a".repeat(MAX_PAYLOAD - 1);
        s.push('é'); // 2 bytes, straddles the cap
        s.push('z');
        let truncated = truncate_for_clipboard(&s);
        assert!(truncated.ends_with("…[truncated]"));
        // No partial UTF-8 char before the marker.
        let stem = truncated.trim_end_matches("…[truncated]");
        assert!(stem.is_char_boundary(stem.len()));
    }

    #[test]
    fn short_payload_is_borrowed_unchanged() {
        let s = "small";
        let out = truncate_for_clipboard(s);
        assert!(matches!(out, std::borrow::Cow::Borrowed(_)));
        assert_eq!(out, "small");
    }
}
