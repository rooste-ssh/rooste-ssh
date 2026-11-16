//! Octal output unescaping, hex input encoding, and DCS `\033P1000p` detection.

use std::fmt::Write;

use crate::ids::TmuxPaneId;

/// The DCS sequence emitted by `tmux -CC` when entering control mode (`\033P1000p`).
pub const DCS_TMUX_ENTER: &[u8] = b"\x1bP1000p";

/// Decode octal-escaped bytes from a `%output` or `%extended-output` notification.
///
/// In tmux control mode, characters `< ASCII 32` and `\` (`\134`) are encoded as
/// a backslash followed by three octal digits (`0..=7`). All other bytes are
/// passed through verbatim so stateful UTF-8 / VT parsers can process multi-byte
/// sequences split across consecutive `%output` notifications.
pub fn decode_tmux_output(escaped: &[u8], out: &mut Vec<u8>) {
    let mut i = 0;
    while i < escaped.len() {
        if escaped[i] == b'\\' && i + 3 < escaped.len() {
            let b0 = escaped[i + 1];
            let b1 = escaped[i + 2];
            let b2 = escaped[i + 3];
            if (b'0'..=b'7').contains(&b0)
                && (b'0'..=b'7').contains(&b1)
                && (b'0'..=b'7').contains(&b2)
            {
                let val = ((b0 - b'0') << 6) | ((b1 - b'0') << 3) | (b2 - b'0');
                out.push(val);
                i += 4;
                continue;
            }
        }
        out.push(escaped[i]);
        i += 1;
    }
}

/// Encode raw terminal input bytes into a `send-keys -t %pane -H <hex...>` command.
pub fn encode_send_keys_hex(pane: TmuxPaneId, bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    let mut cmd = String::with_capacity(24 + bytes.len() * 3);
    let _ = write!(cmd, "send-keys -t %{} -H", pane.0);
    for &b in bytes {
        let _ = write!(cmd, " {:02x}", b);
    }
    cmd.push('\n');
    cmd
}

/// Encode a pasted text string for a target pane, wrapping with bracketed paste
/// escape sequences when the pane's terminal has bracketed paste enabled.
pub fn encode_paste_hex(pane: TmuxPaneId, text: &str, bracketed: bool) -> String {
    if text.is_empty() {
        return String::new();
    }
    let mut payload = Vec::with_capacity(text.len() + 16);
    if bracketed {
        payload.extend_from_slice(b"\x1b[200~");
        for b in text.bytes() {
            if b != b'\x1b' {
                payload.push(b);
            }
        }
        payload.extend_from_slice(b"\x1b[201~");
    } else {
        payload.extend_from_slice(text.as_bytes());
    }
    encode_send_keys_hex(pane, &payload)
}

/// Escape a string for safe inclusion inside double quotes in a tmux command.
pub fn escape_tmux_arg(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        match ch {
            '\\' | '"' | '$' => {
                out.push('\\');
                out.push(ch);
            }
            '\n' | '\r' => out.push(' '),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

/// Streaming detector for the `\x1bP1000p` sequence across arbitrary chunk boundaries.
#[derive(Debug, Clone, Default)]
pub struct DcsDetector {
    matched: usize,
}

impl DcsDetector {
    pub fn new() -> Self {
        Self { matched: 0 }
    }

    pub fn reset(&mut self) {
        self.matched = 0;
    }

    /// Scan `chunk` for `\x1bP1000p`.
    ///
    /// If found, returns `Some((before, after))` where `before` are the bytes
    /// preceding the sequence (excluding any prefix bytes already fed in earlier
    /// calls) and `after` are the bytes immediately following `\x1bP1000p`.
    pub fn scan<'a>(&mut self, chunk: &'a [u8]) -> Option<(&'a [u8], &'a [u8])> {
        for (idx, &b) in chunk.iter().enumerate() {
            if b == DCS_TMUX_ENTER[self.matched] {
                self.matched += 1;
                if self.matched == DCS_TMUX_ENTER.len() {
                    self.matched = 0;
                    let prefix_in_chunk = (idx + 1).saturating_sub(DCS_TMUX_ENTER.len());
                    return Some((&chunk[..prefix_in_chunk], &chunk[idx + 1..]));
                }
            } else if b == DCS_TMUX_ENTER[0] {
                self.matched = 1;
            } else {
                self.matched = 0;
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_octal_and_preserves_utf8() {
        let mut out = Vec::new();
        decode_tmux_output(b"hello\\015\\012world\\134!", &mut out);
        assert_eq!(out, b"hello\r\nworld\\!");

        out.clear();
        decode_tmux_output("你好\\012".as_bytes(), &mut out);
        assert_eq!(String::from_utf8(out).unwrap(), "你好\n");
    }

    #[test]
    fn encodes_hex_send_keys() {
        let cmd = encode_send_keys_hex(TmuxPaneId(3), b"ls\n");
        assert_eq!(cmd, "send-keys -t %3 -H 6c 73 0a\n");
    }

    #[test]
    fn detects_dcs_across_split_chunks() {
        let mut det = DcsDetector::new();
        assert_eq!(det.scan(b"prompt$ \x1bP10"), None);
        let (_, after) = det.scan(b"00p%begin 1 1 0\n").unwrap();
        assert_eq!(after, b"%begin 1 1 0\n");
    }
}
