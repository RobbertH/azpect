//! System clipboard, via two complementary delivery paths.
//!
//! 1. **OSC52 escape** (`ESC ] 52 ; c ; <base64-data> BEL`) — asks the
//!    terminal emulator to copy a payload into the system clipboard. It works
//!    through SSH (the data rides the same channel as the rest of the TUI) and
//!    needs no external binary. Most modern terminals honour it — but some
//!    (Apple Terminal, GNOME Terminal / VTE) silently drop the write, which is
//!    why a second path exists.
//!
//! 2. **Native helper** (`wl-copy` / `xclip` / `xsel` / `pbcopy`) — spawned
//!    when present, to cover those OSC52-deaf local terminals. Pure
//!    best-effort: nothing breaks if no helper is installed, and we add no
//!    build-time dependency on any of them.
//!
//! [`copy`] always does (1) and *also* attempts (2): they're complementary,
//! not either/or. Only (1) reaches the *local* clipboard when you're SSH'd
//! into a remote box (a remote `xclip` would target the remote's headless
//! display); only (2) works in terminals that ignore OSC52.
//!
//! Limitation: many terminals cap the OSC52 payload size (~75 KB in
//! tmux/wezterm/kitty). We pre-truncate at [`MAX_PAYLOAD`] so a 1 MB log line
//! doesn't get silently dropped.

#![allow(dead_code)]

use std::io::{self, Write};

/// Maximum number of *raw* bytes we attempt to copy. Larger payloads are
/// truncated with a "…[truncated]" marker because most terminals reject them.
pub const MAX_PAYLOAD: usize = 64 * 1024;

const OSC52_PREFIX: &str = "\x1b]52;c;";
const OSC52_SUFFIX: &str = "\x07";

/// Copy `text` to the system clipboard. Returns the number of bytes actually
/// sent (post-truncation), or an `io::Error` if writing the OSC52 escape to
/// the terminal failed.
///
/// Emits the OSC52 escape *and* — off-thread, best-effort — pushes the same
/// payload to a native clipboard helper for terminals that ignore OSC52. A
/// missing or failing helper is not an error: OSC52 may still have worked, and
/// whether *either* path was honoured is not knowable from inside the program.
/// The caller should display a friendly status hint and let the user verify by
/// pasting.
pub fn copy(text: &str) -> io::Result<usize> {
    let n = copy_to(io::stdout().lock(), text)?;
    // Hand the same payload to a native helper on a detached thread: xclip /
    // xsel / wl-copy fork to keep serving the selection, so doing this inline
    // could stall the UI on a misbehaving helper.
    spawn_helper_copy(truncate_for_clipboard(text).into_owned().into_bytes());
    Ok(n)
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

/// Fire-and-forget the native-helper copy on a detached thread so it can never
/// block the UI (see [`try_helper`] for why a helper might not return quickly).
fn spawn_helper_copy(payload: Vec<u8>) {
    std::thread::spawn(move || copy_via_helper(&payload));
}

/// Native clipboard helpers, in priority order: Wayland, X11, X11, macOS.
/// The first one present on `PATH` wins.
const HELPERS: &[(&str, &[&str])] = &[
    ("wl-copy", &[]),
    ("xclip", &["-selection", "clipboard"]),
    ("xsel", &["--clipboard", "--input"]),
    ("pbcopy", &[]),
];

/// Best-effort copy through whichever native helper is installed. Does nothing
/// if none are on `PATH`; the OSC52 write in [`copy`] is the universal fallback
/// (and the only path that reaches the *local* clipboard over SSH).
fn copy_via_helper(payload: &[u8]) {
    for (bin, args) in HELPERS {
        match try_helper(bin, args, payload) {
            Ok(()) => return,
            // Not installed — try the next candidate.
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            // Present but failed (e.g. no display) — stop; OSC52 covers us.
            Err(_) => return,
        }
    }
}

/// Spawn `bin`, feed `payload` to its stdin, and reap it. Returns the spawn
/// error (notably [`io::ErrorKind::NotFound`] when `bin` isn't on `PATH`) so
/// the caller can fall through to the next candidate.
fn try_helper(bin: &str, args: &[&str], payload: &[u8]) -> io::Result<()> {
    use std::process::{Command, Stdio};
    let mut child = Command::new(bin)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(payload)?;
        // Drop `stdin` here → EOF, so the helper stops reading.
    }
    // xclip / xsel / wl-copy fork to own the selection, so this returns
    // promptly; pbcopy exits after reading. Either way, reap to avoid a zombie.
    child.wait()?;
    Ok(())
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
    fn missing_helper_reports_not_found() {
        // The probe loop relies on NotFound to fall through to the next
        // candidate, so a binary that can't exist must surface that kind.
        let err = try_helper("azpect-no-such-clipboard-binary", &[], b"x").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn short_payload_is_borrowed_unchanged() {
        let s = "small";
        let out = truncate_for_clipboard(s);
        assert!(matches!(out, std::borrow::Cow::Borrowed(_)));
        assert_eq!(out, "small");
    }
}
