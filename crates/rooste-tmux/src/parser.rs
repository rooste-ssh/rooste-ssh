//! Line-oriented streaming parser for tmux control mode (`tmux -CC`).

use crate::ids::{TmuxPaneId, TmuxSessionId, TmuxWindowId};
use crate::layout::{LayoutNode, parse_layout};
use crate::unescape::decode_tmux_output;

/// An asynchronous notification sent by the tmux server (`%...`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TmuxNotification {
    /// `%output %pane <octal-escaped-bytes>` (decoded into raw bytes).
    Output { pane: TmuxPaneId, data: Vec<u8> },
    /// `%extended-output %pane <age_ms> : <octal-escaped-bytes>` (decoded into raw bytes).
    ExtendedOutput {
        pane: TmuxPaneId,
        age_ms: u64,
        data: Vec<u8>,
    },
    /// `%window-add @window`
    WindowAdd { window: TmuxWindowId },
    /// `%window-close @window` or `%unlinked-window-close @window`
    WindowClose { window: TmuxWindowId },
    /// `%pane-exited %pane`
    PaneExited { pane: TmuxPaneId },
    /// `%window-renamed @window <new-name>`
    WindowRenamed { window: TmuxWindowId, name: String },
    /// `%window-pane-changed @window %pane`
    WindowPaneChanged {
        window: TmuxWindowId,
        pane: TmuxPaneId,
    },
    /// `%layout-change @window <layout> <visible-layout> <flags>`
    LayoutChange {
        window: TmuxWindowId,
        layout: LayoutNode,
        visible_layout: Option<LayoutNode>,
        flags: String,
    },
    /// `%session-changed $session <name>`
    SessionChanged {
        session: TmuxSessionId,
        name: String,
    },
    /// `%session-window-changed $session @window`
    SessionWindowChanged {
        session: TmuxSessionId,
        window: TmuxWindowId,
    },
    /// `%sessions-changed`
    SessionsChanged,
    /// `%pause %pane`
    Pause { pane: TmuxPaneId },
    /// `%continue %pane`
    Continue { pane: TmuxPaneId },
    /// `%subscription-changed <name> $session @window <idx> %pane : <value>`
    SubscriptionChanged {
        name: String,
        session: Option<TmuxSessionId>,
        window: Option<TmuxWindowId>,
        pane: Option<TmuxPaneId>,
        value: String,
    },
    /// `%exit [reason]`
    Exit { reason: Option<String> },
}

/// A completed command response block delimited by `%begin` and `%end` / `%error`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxResponse {
    pub timestamp: u64,
    pub command_num: u64,
    pub flags: u32,
    pub lines: Vec<String>,
    pub is_error: bool,
}

/// An event produced by [`ControlParser`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TmuxEvent {
    Notification(TmuxNotification),
    Response(TmuxResponse),
    /// Raw protocol line for diagnostic logging in the Gateway tab.
    ProtocolLine(String),
}

#[derive(Debug, Clone)]
struct ActiveBlock {
    timestamp: u64,
    command_num: u64,
    flags: u32,
    lines: Vec<String>,
}

/// Streaming line parser for tmux control mode.
#[derive(Debug, Default)]
pub struct ControlParser {
    line_buf: Vec<u8>,
    active_block: Option<ActiveBlock>,
    pub log_lines: bool,
}

impl ControlParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed raw bytes read from the SSH transport into the parser.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<TmuxEvent> {
        let mut events = Vec::new();
        for &b in bytes {
            if b == b'\n' {
                if self.line_buf.last() == Some(&b'\r') {
                    self.line_buf.pop();
                }
                let raw_line = std::mem::take(&mut self.line_buf);
                self.process_line(&raw_line, &mut events);
            } else {
                self.line_buf.push(b);
            }
        }
        events
    }

    fn process_line(&mut self, raw: &[u8], events: &mut Vec<TmuxEvent>) {
        // Strip leading DCS `\x1bP1000p` or ST `\x1b\\` sequences if present on the same line.
        let mut slice = raw;
        if let Some(rest) = slice.strip_prefix(b"\x1bP1000p") {
            slice = rest;
        }
        if let Some(rest) = slice.strip_prefix(b"\x1b\\") {
            slice = rest;
        }
        if let Some(rest) = slice.strip_suffix(b"\x1b\\") {
            slice = rest;
        }
        if slice.is_empty() {
            return;
        }

        let line_lossy = String::from_utf8_lossy(slice);
        if self.log_lines && !slice.starts_with(b"%output ") {
            events.push(TmuxEvent::ProtocolLine(line_lossy.into_owned()));
        }

        // Fast path for `%output %<id> <escaped>` using raw bytes before UTF-8 conversion.
        if let Some(rest) = slice.strip_prefix(b"%output ")
            && let Some(notif) = parse_output_line(rest)
        {
            events.push(TmuxEvent::Notification(notif));
            return;
        }
        if let Some(rest) = slice.strip_prefix(b"%extended-output ")
            && let Some(notif) = parse_extended_output_line(rest)
        {
            events.push(TmuxEvent::Notification(notif));
            return;
        }

        let line = String::from_utf8_lossy(slice);
        let line_ref = line.as_ref();

        if let Some(args) = line_ref.strip_prefix("%begin ")
            && let Some((timestamp, command_num, flags)) = parse_block_header(args)
        {
            self.active_block = Some(ActiveBlock {
                timestamp,
                command_num,
                flags,
                lines: Vec::new(),
            });
            return;
        }

        if let Some(args) = line_ref.strip_prefix("%end ")
            && let Some((timestamp, command_num, flags)) = parse_block_header(args)
            && let Some(block) = self.active_block.take()
        {
            events.push(TmuxEvent::Response(TmuxResponse {
                timestamp: timestamp.max(block.timestamp),
                command_num: if command_num != 0 {
                    command_num
                } else {
                    block.command_num
                },
                flags: flags.max(block.flags),
                lines: block.lines,
                is_error: false,
            }));
            return;
        }

        if let Some(args) = line_ref.strip_prefix("%error ")
            && let Some((timestamp, command_num, flags)) = parse_block_header(args)
            && let Some(block) = self.active_block.take()
        {
            events.push(TmuxEvent::Response(TmuxResponse {
                timestamp: timestamp.max(block.timestamp),
                command_num: if command_num != 0 {
                    command_num
                } else {
                    block.command_num
                },
                flags: flags.max(block.flags),
                lines: block.lines,
                is_error: true,
            }));
            return;
        }

        // Asynchronous notifications can arrive both outside and inside `%begin..%end`.
        if line_ref.starts_with('%')
            && let Some(notif) = parse_notification(line_ref)
        {
            events.push(TmuxEvent::Notification(notif));
            return;
        }

        if let Some(block) = &mut self.active_block {
            block.lines.push(line_ref.to_owned());
        }
    }
}

fn parse_block_header(args: &str) -> Option<(u64, u64, u32)> {
    let mut parts = args.split_whitespace();
    let ts = parts.next()?.parse::<u64>().ok()?;
    let num = parts.next()?.parse::<u64>().ok()?;
    let flags = parts
        .next()
        .and_then(|f| f.parse::<u32>().ok())
        .unwrap_or(1);
    Some((ts, num, flags))
}

fn parse_output_line(rest: &[u8]) -> Option<TmuxNotification> {
    let space = rest.iter().position(|&b| b == b' ')?;
    let pane_str = std::str::from_utf8(&rest[..space]).ok()?;
    let pane = pane_str.parse::<TmuxPaneId>().ok()?;
    let mut data = Vec::with_capacity(rest.len() - space - 1);
    decode_tmux_output(&rest[space + 1..], &mut data);
    Some(TmuxNotification::Output { pane, data })
}

fn parse_extended_output_line(rest: &[u8]) -> Option<TmuxNotification> {
    // Format: `%pane <age_ms> ... : <data>`
    let colon_idx = rest.windows(3).position(|w| w == b" : ")?;
    let header = std::str::from_utf8(&rest[..colon_idx]).ok()?;
    let mut parts = header.split_whitespace();
    let pane = parts.next()?.parse::<TmuxPaneId>().ok()?;
    let age_ms = parts
        .next()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    let payload = &rest[colon_idx + 3..];
    let mut data = Vec::with_capacity(payload.len());
    decode_tmux_output(payload, &mut data);
    Some(TmuxNotification::ExtendedOutput { pane, age_ms, data })
}

fn parse_notification(line: &str) -> Option<TmuxNotification> {
    if line == "%sessions-changed" {
        return Some(TmuxNotification::SessionsChanged);
    }
    if line == "%exit" {
        return Some(TmuxNotification::Exit { reason: None });
    }
    if let Some(reason) = line.strip_prefix("%exit ") {
        let reason = reason.trim();
        return Some(TmuxNotification::Exit {
            reason: (!reason.is_empty()).then(|| reason.to_owned()),
        });
    }
    if let Some(arg) = line.strip_prefix("%window-add ") {
        let mut parts = arg.split_whitespace();
        return Some(TmuxNotification::WindowAdd {
            window: parts.next()?.parse().ok()?,
        });
    }
    if let Some(arg) = line
        .strip_prefix("%window-close ")
        .or_else(|| line.strip_prefix("%unlinked-window-close "))
    {
        let mut parts = arg.split_whitespace();
        return Some(TmuxNotification::WindowClose {
            window: parts.next()?.parse().ok()?,
        });
    }
    if let Some(arg) = line.strip_prefix("%pane-exited ") {
        let mut parts = arg.split_whitespace();
        return Some(TmuxNotification::PaneExited {
            pane: parts.next()?.parse().ok()?,
        });
    }
    if let Some(rest) = line.strip_prefix("%window-renamed ") {
        let (win_str, name) = rest.split_once(' ')?;
        return Some(TmuxNotification::WindowRenamed {
            window: win_str.parse().ok()?,
            name: name.to_owned(),
        });
    }
    if let Some(rest) = line.strip_prefix("%window-pane-changed ") {
        let mut parts = rest.split_whitespace();
        let window = parts.next()?.parse().ok()?;
        let pane = parts.next()?.parse().ok()?;
        return Some(TmuxNotification::WindowPaneChanged { window, pane });
    }
    if let Some(rest) = line.strip_prefix("%layout-change ") {
        let mut parts = rest.split_whitespace();
        let window = parts.next()?.parse().ok()?;
        let layout_str = parts.next()?;
        let layout = parse_layout(layout_str).ok()?;
        let visible_layout = parts.next().and_then(|s| parse_layout(s).ok());
        let flags = parts.next().unwrap_or_default().to_owned();
        return Some(TmuxNotification::LayoutChange {
            window,
            layout,
            visible_layout,
            flags,
        });
    }
    if let Some(rest) = line.strip_prefix("%session-changed ") {
        let (sess_str, name) = rest.split_once(' ')?;
        return Some(TmuxNotification::SessionChanged {
            session: sess_str.parse().ok()?,
            name: name.to_owned(),
        });
    }
    if let Some(rest) = line.strip_prefix("%session-window-changed ") {
        let mut parts = rest.split_whitespace();
        let session = parts.next()?.parse().ok()?;
        let window = parts.next()?.parse().ok()?;
        return Some(TmuxNotification::SessionWindowChanged { session, window });
    }
    if let Some(arg) = line.strip_prefix("%pause ") {
        return Some(TmuxNotification::Pause {
            pane: arg.trim().parse().ok()?,
        });
    }
    if let Some(arg) = line.strip_prefix("%continue ") {
        return Some(TmuxNotification::Continue {
            pane: arg.trim().parse().ok()?,
        });
    }
    if let Some(rest) = line.strip_prefix("%subscription-changed ") {
        let (header, value) = rest.split_once(" : ")?;
        let mut parts = header.split_whitespace();
        let name = parts.next()?.to_owned();
        let session = parts.next().and_then(|s| s.parse().ok());
        let window = parts.next().and_then(|s| s.parse().ok());
        let _idx = parts.next();
        let pane = parts.next().and_then(|s| s.parse().ok());
        return Some(TmuxNotification::SubscriptionChanged {
            name,
            session,
            window,
            pane,
            value: value.to_owned(),
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_command_blocks_and_notifications() {
        let mut parser = ControlParser::new();
        let stream = b"%begin 1578920019 258 0\n\
                       @1\tbash\t1\tb25f,80x24,0,0,0\n\
                       %end 1578920019 258 0\n\
                       %window-add @1\n\
                       %session-changed $1 main\n\
                       %output %0 hello\\015\\012\n\
                       %exit detached\n";
        let events = parser.feed(stream);
        assert_eq!(events.len(), 5);
        assert!(matches!(
            &events[0],
            TmuxEvent::Response(r) if r.command_num == 258 && r.lines.len() == 1 && !r.is_error
        ));
        assert_eq!(
            events[1],
            TmuxEvent::Notification(TmuxNotification::WindowAdd {
                window: TmuxWindowId(1)
            })
        );
        assert_eq!(
            events[2],
            TmuxEvent::Notification(TmuxNotification::SessionChanged {
                session: TmuxSessionId(1),
                name: "main".into()
            })
        );
        assert_eq!(
            events[3],
            TmuxEvent::Notification(TmuxNotification::Output {
                pane: TmuxPaneId(0),
                data: b"hello\r\n".to_vec()
            })
        );
        assert_eq!(
            events[4],
            TmuxEvent::Notification(TmuxNotification::Exit {
                reason: Some("detached".into())
            })
        );
    }

    #[test]
    fn parses_pane_exited_and_unlinked_close() {
        let mut parser = ControlParser::new();
        let stream = b"%pane-exited %3 0\n%unlinked-window-close @5\n";
        let events = parser.feed(stream);
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0],
            TmuxEvent::Notification(TmuxNotification::PaneExited {
                pane: TmuxPaneId(3)
            })
        );
        assert_eq!(
            events[1],
            TmuxEvent::Notification(TmuxNotification::WindowClose {
                window: TmuxWindowId(5)
            })
        );
    }
}
