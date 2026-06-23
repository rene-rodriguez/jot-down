//! Terminal clipboard support via the OSC 52 escape sequence.
//!
//! OSC 52 lets a terminal application set the system clipboard by writing
//! `ESC ] 52 ; c ; <base64> BEL` to the terminal. It needs no platform
//! clipboard library, works over SSH, and doesn't disturb the alternate screen
//! — a good fit for Jot's offline, terminal-first design. Support varies by
//! terminal, so a copy is best-effort; callers surface a status either way.

use std::io::{self, Write};

/// Copy `text` to the system clipboard using OSC 52. Best-effort: returns the
/// write error if stdout can't be written, but a terminal silently ignoring the
/// sequence is indistinguishable from success.
pub fn copy_to_clipboard(text: &str) -> io::Result<()> {
    let encoded = base64_encode(text.as_bytes());
    let seq = format!("\x1b]52;c;{encoded}\x07");
    let mut stdout = io::stdout();
    stdout.write_all(seq.as_bytes())?;
    stdout.flush()
}

/// Standard base64 (RFC 4648) encoding with `=` padding. Hand-rolled to avoid a
/// dependency for this single use.
fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);

    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;

        out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[((n >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 0x3f) as usize] as char
        } else {
            '='
        });
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_handles_high_bytes() {
        // "≈" is 0xE2 0x89 0x88 in UTF-8.
        assert_eq!(base64_encode("≈".as_bytes()), "4omI");
    }
}
