//! Sans-I/O state machine tracking tmux sessions, windows, panes, and pending commands.

use std::collections::{BTreeMap, VecDeque};

use crate::ids::{TmuxPaneId, TmuxSessionId, TmuxWindowId};
use crate::layout::{LayoutNode, parse_layout};
use crate::parser::{ControlParser, TmuxEvent, TmuxNotification, TmuxResponse};
use crate::unescape::escape_tmux_arg;

/// Separates fields in the `-F` formats we ask tmux for.
///
/// It must be printable ASCII: without a UTF-8 locale on the server, tmux
/// replaces tabs and other control characters in control-mode replies with `_`.
/// Free-text fields (names, paths, titles) come last, so only the final field
/// could ever contain it, and `splitn` keeps that field whole.
const FIELD_SEP: &str = "|~|";

/// A random number from the standard library's randomly keyed hasher, which
/// is plenty to keep markers unguessable without a dependency.
fn random_u64() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish()
}

/// Undo tmux's `#{q:…}` quoting, which puts a backslash before special
/// characters. The path is quoted so that a `|~|` in it can't be mistaken for
/// [`FIELD_SEP`]: every `|` in it arrives as `\|`.
fn unquote(quoted: &str) -> String {
    let mut out = String::with_capacity(quoted.len());
    let mut chars = quoted.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => out.extend(chars.next()),
            c => out.push(c),
        }
    }
    out
}

/// Snapshot of a remote tmux pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxPaneSnapshot {
    pub window_id: TmuxWindowId,
    pub pane_id: TmuxPaneId,
    pub active: bool,
    pub cols: u16,
    pub rows: u16,
    pub title: String,
    pub cwd: String,
}

/// Snapshot of a remote tmux window and its split layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxWindowSnapshot {
    pub id: TmuxWindowId,
    pub name: String,
    pub active: bool,
    pub layout: Option<LayoutNode>,
    pub panes: Vec<TmuxPaneSnapshot>,
}

/// High-level UI actions translated into tmux control commands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TmuxClientAction {
    /// Open a new tmux window in the attached session.
    NewWindow { cwd: Option<String> },
    /// Split `pane` horizontally (left/right, `split-window -h`).
    SplitHorizontal {
        pane: TmuxPaneId,
        cwd: Option<String>,
    },
    /// Split `pane` vertically (top/bottom, `split-window -v`).
    SplitVertical {
        pane: TmuxPaneId,
        cwd: Option<String>,
    },
    /// Kill a single tmux pane (`kill-pane -t %id`).
    KillPane { pane: TmuxPaneId },
    /// Kill an entire tmux window (`kill-window -t @id`).
    KillWindow { window: TmuxWindowId },
    /// Switch the active window (`select-window -t @id`).
    SelectWindow { window: TmuxWindowId },
    /// Focus a specific pane (`select-pane -t %id`).
    SelectPane { pane: TmuxPaneId },
    /// Rename a window (`rename-window -t @id <name>`).
    RenameWindow { window: TmuxWindowId, name: String },
    /// Resume a flow-control paused pane (`refresh-client -A '%id:continue'`).
    ContinuePane { pane: TmuxPaneId },
    /// Toggle `synchronize-panes` on a window.
    ToggleSyncPanes { window: TmuxWindowId },
    /// Execute a raw user-provided tmux command.
    RawCommand { command: String },
}

impl TmuxClientAction {
    /// Format the action as a newline-terminated tmux command string.
    pub fn to_command_string(&self) -> String {
        match self {
            Self::NewWindow { cwd } => match cwd.as_deref().filter(|s| !s.is_empty()) {
                Some(dir) => format!("new-window -c {}\n", escape_tmux_arg(dir)),
                None => "new-window\n".to_owned(),
            },
            Self::SplitHorizontal { pane, cwd } => match cwd.as_deref().filter(|s| !s.is_empty()) {
                Some(dir) => format!("split-window -h -t {} -c {}\n", pane, escape_tmux_arg(dir)),
                None => format!("split-window -h -t {}\n", pane),
            },
            Self::SplitVertical { pane, cwd } => match cwd.as_deref().filter(|s| !s.is_empty()) {
                Some(dir) => format!("split-window -v -t {} -c {}\n", pane, escape_tmux_arg(dir)),
                None => format!("split-window -v -t {}\n", pane),
            },
            Self::KillPane { pane } => format!("kill-pane -t {}\n", pane),
            Self::KillWindow { window } => format!("kill-window -t {}\n", window),
            Self::SelectWindow { window } => format!("select-window -t {}\n", window),
            Self::SelectPane { pane } => format!("select-pane -t {}\n", pane),
            Self::RenameWindow { window, name } => {
                format!("rename-window -t {} {}\n", window, escape_tmux_arg(name))
            }
            Self::ContinuePane { pane } => {
                format!("refresh-client -A '{}:continue'\n", pane)
            }
            Self::ToggleSyncPanes { window } => {
                format!("set-window-option -t {} synchronize-panes\n", window)
            }
            Self::RawCommand { command } => {
                // One line only: a newline would end the command early.
                let line = command.lines().map(str::trim).collect::<Vec<_>>().join(" ");
                let line = line.trim();
                if line.is_empty() {
                    String::new()
                } else {
                    format!("{line}\n")
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PendingCmd {
    /// Initial guard `%begin..%end` emitted by `tmux -CC` when attaching.
    InitialAttach,
    RefreshClientSize,
    RefreshClientFlags,
    SubscribeFormats,
    ListWindows,
    ListPanes {
        finalize_attach: bool,
    },
    CapturePane {
        pane: TmuxPaneId,
    },
    UserCommand,
    /// A command typed by the user, which may run any number of commands
    /// (`a ; b`, `if-shell … { … }`), each with its own reply block. Every
    /// reply is the user's until the marker sent after it comes back.
    UserCommandsUntil {
        marker: String,
    },
}

/// High-level output produced by [`TmuxMachine`] after processing incoming bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MachineEvent {
    /// Commands that must be written back to the remote SSH transport immediately.
    SendCommands(String),
    /// Initial attach or topology refresh completed with the full window/pane tree.
    TopologySynced {
        session_id: TmuxSessionId,
        session_name: String,
        windows: Vec<TmuxWindowSnapshot>,
    },
    /// Decoded VT output for a specific pane (either initial capture or live `%output`).
    PaneOutput { pane_id: TmuxPaneId, bytes: Vec<u8> },
    /// Incremental notification (`%window-add`, `%window-close`, `%layout-change`, `%pause`, etc.).
    Notification(TmuxNotification),
    /// Pane metadata updated via format subscription (`title`, `cwd`).
    PaneMetadata {
        pane_id: TmuxPaneId,
        title: String,
        cwd: String,
    },
    /// Diagnostic protocol log line.
    ProtocolLog(String),
    /// Control mode exited cleanly or with a reason and any trailing output.
    Detached {
        reason: Option<String>,
        trailing: Vec<u8>,
    },
}

/// Pure sans-I/O state machine managing a `tmux -CC` control session.
#[derive(Debug)]
pub struct TmuxMachine {
    parser: ControlParser,
    pending: VecDeque<PendingCmd>,
    pub session_id: TmuxSessionId,
    pub session_name: String,
    pub windows: BTreeMap<TmuxWindowId, TmuxWindowSnapshot>,
    pub panes: BTreeMap<TmuxPaneId, TmuxPaneSnapshot>,
    staged_windows: Option<BTreeMap<TmuxWindowId, TmuxWindowSnapshot>>,
    scrollback_capture: usize,
    pause_after_secs: u16,
    attached_once: bool,
    /// Error text tmux reported for its own attach command, shown on `%exit`.
    attach_error: Option<String>,
    /// Client size sent with `refresh-client -C` once attached.
    client_size: (u16, u16),
    /// tmux accepted the attach and the bootstrap commands were queued.
    ready: bool,
    /// Counter that makes each [`PendingCmd::UserCommandsUntil`] marker unique.
    next_marker: u64,
    /// Random part of the markers, so a typed command can't print one.
    marker_nonce: u64,
}

impl TmuxMachine {
    /// Create a new state machine for a `tmux -CC` client that just started.
    ///
    /// Nothing is sent yet: tmux first answers its own attach command, and the
    /// bootstrap commands go out (as [`MachineEvent::SendCommands`]) only once
    /// that succeeded. If the attach fails, tmux exits straight away and
    /// anything sent would land in the user's shell instead.
    pub fn new(
        client_cols: u16,
        client_rows: u16,
        scrollback_capture: usize,
        pause_after_secs: u16,
    ) -> Self {
        let mut parser = ControlParser::new();
        parser.log_lines = true;
        let mut pending = VecDeque::new();
        // When `tmux -CC` starts, tmux emits an unsolicited `%begin..%end` for the
        // `new-session` or `attach-session` command itself before our commands run.
        pending.push_back(PendingCmd::InitialAttach);

        Self {
            parser,
            pending,
            session_id: TmuxSessionId(0),
            session_name: "tmux".to_owned(),
            windows: BTreeMap::new(),
            panes: BTreeMap::new(),
            staged_windows: None,
            scrollback_capture: if scrollback_capture == 0 {
                0
            } else {
                scrollback_capture.clamp(100, 50_000)
            },
            pause_after_secs: pause_after_secs.max(1),
            attached_once: false,
            attach_error: None,
            client_size: (client_cols.max(20), client_rows.max(5)),
            ready: false,
            next_marker: 0,
            marker_nonce: random_u64(),
        }
    }

    /// Prepare a command the user typed, returning the text to send.
    ///
    /// tmux replies to each command the line runs, and how many that is can't
    /// be told from the text, so a `display-message` marker follows it.
    pub fn queue_raw_command(&mut self, command: &str) -> String {
        let cmd = TmuxClientAction::RawCommand {
            command: command.to_owned(),
        }
        .to_command_string();
        if cmd.is_empty() {
            return cmd;
        }
        self.next_marker += 1;
        let marker = format!(
            "rooste-sync-{:016x}-{}",
            self.marker_nonce, self.next_marker
        );
        let text = format!("{cmd}display-message -p {marker}\n");
        self.pending
            .push_back(PendingCmd::UserCommandsUntil { marker });
        text
    }

    /// Whether tmux accepted the attach, so commands may be sent to it.
    ///
    /// Until then, anything written could reach the shell instead (if the
    /// attach fails), so callers must hold their commands back.
    pub fn is_ready(&self) -> bool {
        self.ready
    }

    /// Remember the client size to send once attached.
    pub fn set_client_size(&mut self, cols: u16, rows: u16) {
        self.client_size = (cols.max(20), rows.max(5));
    }

    /// Queue the commands that set up the client and load the session's state.
    fn queue_bootstrap(&mut self, out: &mut String) {
        self.ready = true;
        let (cols, rows) = self.client_size;
        self.queue_command(
            PendingCmd::RefreshClientSize,
            &format!("refresh-client -C {cols}x{rows}\n"),
            out,
        );
        self.queue_command(
            PendingCmd::RefreshClientFlags,
            &format!("refresh-client -f pause-after={}\n", self.pause_after_secs),
            out,
        );
        self.queue_command(
            PendingCmd::SubscribeFormats,
            "refresh-client -B \"rooste_meta:%*:#{q:pane_current_path}|~|#{pane_title}\"\n",
            out,
        );
        self.queue_sync_topology(out, true);
    }

    fn queue_command(&mut self, kind: PendingCmd, raw: &str, out: &mut String) {
        self.pending.push_back(kind);
        out.push_str(raw);
    }

    /// Queue `list-windows` and `list-panes` to reconcile full session state.
    pub fn queue_sync_topology(&mut self, out: &mut String, finalize_attach: bool) {
        self.queue_command(
            PendingCmd::ListWindows,
            "list-windows -F \"#{window_id}|~|#{window_active}|~|#{window_layout}|~|#{window_name}\"\n",
            out,
        );
        self.queue_command(
            PendingCmd::ListPanes { finalize_attach },
            "list-panes -s -F \"#{window_id}|~|#{pane_id}|~|#{pane_active}|~|#{pane_width}|~|#{pane_height}|~|#{q:pane_current_path}|~|#{pane_title}\"\n",
            out,
        );
    }

    /// Register an outgoing user/UI command so its `%begin..%end` block is matched.
    pub fn register_user_command(&mut self) {
        self.pending.push_back(PendingCmd::UserCommand);
    }

    /// Feed incoming transport bytes and collect resulting state machine events.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<MachineEvent> {
        let mut out = Vec::new();
        let mut follow_up_cmds = String::new();

        for event in self.parser.feed(bytes) {
            match event {
                TmuxEvent::ProtocolLine(line) => {
                    out.push(MachineEvent::ProtocolLog(line));
                }
                TmuxEvent::Response(resp) => {
                    self.handle_response(resp, &mut out, &mut follow_up_cmds);
                }
                TmuxEvent::Notification(notif) => {
                    self.handle_notification(notif, &mut out, &mut follow_up_cmds);
                }
            }
        }

        // After `%exit` the shell is reading again, so nothing more may be sent.
        let exited = out
            .iter()
            .any(|e| matches!(e, MachineEvent::Detached { .. }));
        if !follow_up_cmds.is_empty() && !exited {
            out.insert(0, MachineEvent::SendCommands(follow_up_cmds));
        }
        out
    }

    fn handle_response(
        &mut self,
        resp: TmuxResponse,
        out: &mut Vec<MachineEvent>,
        follow_up_cmds: &mut String,
    ) {
        if let Some(PendingCmd::UserCommandsUntil { marker }) = self.pending.front() {
            if !resp.is_error && resp.lines.len() == 1 && resp.lines[0] == *marker {
                self.pending.pop_front();
            } else if resp.is_error {
                out.push(MachineEvent::ProtocolLog(format!(
                    "tmux command error: {}",
                    resp.lines.join(" | ")
                )));
            }
            return;
        }
        let cmd = self.pending.pop_front().unwrap_or(PendingCmd::UserCommand);
        if cmd == PendingCmd::InitialAttach {
            if resp.is_error {
                self.attach_error = Some(resp.lines.join(" ")).filter(|e| !e.is_empty());
            } else {
                self.queue_bootstrap(follow_up_cmds);
            }
            return;
        }
        if resp.is_error {
            self.staged_windows = None;
            out.push(MachineEvent::ProtocolLog(format!(
                "tmux command error: {}",
                resp.lines.join(" | ")
            )));
            return;
        }
        match cmd {
            PendingCmd::ListWindows => {
                let mut staged = BTreeMap::new();
                for line in &resp.lines {
                    let mut parts = line.splitn(4, FIELD_SEP);
                    let Some(win_id) = parts.next().and_then(|s| s.parse::<TmuxWindowId>().ok())
                    else {
                        continue;
                    };
                    let active = parts.next() == Some("1");
                    let layout = parts.next().and_then(|s| parse_layout(s).ok());
                    let name = parts.next().unwrap_or("shell").to_owned();
                    staged.insert(
                        win_id,
                        TmuxWindowSnapshot {
                            id: win_id,
                            name,
                            active,
                            layout,
                            panes: Vec::new(),
                        },
                    );
                }
                self.staged_windows = Some(staged);
            }
            PendingCmd::ListPanes { finalize_attach } => {
                let Some(mut staged_windows) = self.staged_windows.take() else {
                    return;
                };
                let previous_panes: std::collections::HashSet<TmuxPaneId> =
                    self.panes.keys().copied().collect();
                let mut new_panes = BTreeMap::new();
                for line in &resp.lines {
                    let mut parts = line.splitn(7, FIELD_SEP);
                    let Some(window_id) = parts.next().and_then(|s| s.parse::<TmuxWindowId>().ok())
                    else {
                        continue;
                    };
                    let Some(pane_id) = parts.next().and_then(|s| s.parse::<TmuxPaneId>().ok())
                    else {
                        continue;
                    };
                    let active = parts.next() == Some("1");
                    let cols = parts
                        .next()
                        .and_then(|s| s.parse::<u16>().ok())
                        .unwrap_or(80);
                    let rows = parts
                        .next()
                        .and_then(|s| s.parse::<u16>().ok())
                        .unwrap_or(24);
                    let cwd = unquote(parts.next().unwrap_or_default());
                    let title = parts.next().unwrap_or_default().to_owned();

                    let snap = TmuxPaneSnapshot {
                        window_id,
                        pane_id,
                        active,
                        cols,
                        rows,
                        title,
                        cwd,
                    };
                    new_panes.insert(pane_id, snap.clone());
                    if let Some(win) = staged_windows.get_mut(&window_id) {
                        win.panes.push(snap);
                    }
                    // Capture initial scrollback for newly discovered panes.
                    if (!self.attached_once || !previous_panes.contains(&pane_id))
                        && self.scrollback_capture > 0
                    {
                        self.queue_command(
                            PendingCmd::CapturePane { pane: pane_id },
                            &format!(
                                "capture-pane -p -e -J -S -{} -t {}\n",
                                self.scrollback_capture, pane_id
                            ),
                            follow_up_cmds,
                        );
                    }
                }
                self.windows = staged_windows;
                self.panes = new_panes;
                if finalize_attach || !self.attached_once {
                    self.attached_once = true;
                }
                out.push(MachineEvent::TopologySynced {
                    session_id: self.session_id,
                    session_name: self.session_name.clone(),
                    windows: self.windows.values().cloned().collect(),
                });
            }
            PendingCmd::CapturePane { pane } => {
                if !resp.lines.is_empty() {
                    let mut buf = Vec::new();
                    for (idx, line) in resp.lines.iter().enumerate() {
                        if idx > 0 {
                            buf.extend_from_slice(b"\r\n");
                        }
                        buf.extend_from_slice(line.as_bytes());
                    }
                    buf.extend_from_slice(b"\r\n");
                    out.push(MachineEvent::PaneOutput {
                        pane_id: pane,
                        bytes: buf,
                    });
                }
            }
            PendingCmd::InitialAttach
            | PendingCmd::RefreshClientSize
            | PendingCmd::RefreshClientFlags
            | PendingCmd::SubscribeFormats
            | PendingCmd::UserCommand
            | PendingCmd::UserCommandsUntil { .. } => {}
        }
    }

    fn handle_notification(
        &mut self,
        notif: TmuxNotification,
        out: &mut Vec<MachineEvent>,
        follow_up_cmds: &mut String,
    ) {
        match notif {
            TmuxNotification::Output { pane, data }
            | TmuxNotification::ExtendedOutput { pane, data, .. } => {
                out.push(MachineEvent::PaneOutput {
                    pane_id: pane,
                    bytes: data,
                });
            }
            TmuxNotification::SessionChanged { session, ref name } => {
                self.session_id = session;
                self.session_name = name.clone();
                if self.attached_once {
                    self.queue_sync_topology(follow_up_cmds, false);
                }
                out.push(MachineEvent::Notification(notif));
            }
            TmuxNotification::WindowAdd { .. } => {
                if self.attached_once {
                    self.queue_sync_topology(follow_up_cmds, false);
                }
                out.push(MachineEvent::Notification(notif));
            }
            TmuxNotification::WindowClose { window: _ } => {
                // If sessions A and B share a window and B unlinks it, tmux 3.5a can send %window-close
                // to A while the window is still linked to A. Trigger a topology sync to verify window membership
                // instead of prematurely dropping it.
                if self.attached_once {
                    self.queue_sync_topology(follow_up_cmds, false);
                }
                out.push(MachineEvent::Notification(notif));
            }
            TmuxNotification::PaneExited { pane } => {
                self.panes.remove(&pane);
                for win in self.windows.values_mut() {
                    win.panes.retain(|p| p.pane_id != pane);
                }
                out.push(MachineEvent::Notification(notif));
            }
            TmuxNotification::WindowRenamed { window, ref name } => {
                if let Some(win) = self.windows.get_mut(&window) {
                    win.name = name.clone();
                }
                out.push(MachineEvent::Notification(notif));
            }
            TmuxNotification::LayoutChange {
                window, ref layout, ..
            } => {
                let mut panes_changed = false;
                let layout_panes = layout.panes();
                let layout_pane_ids: std::collections::HashSet<TmuxPaneId> =
                    layout_panes.iter().map(|(id, _)| *id).collect();

                for (pane_id, geom) in &layout_panes {
                    if let Some(pane) = self.panes.get_mut(pane_id) {
                        pane.cols = geom.cols;
                        pane.rows = geom.rows;
                    } else {
                        panes_changed = true;
                    }
                }
                if let Some(win) = self.windows.get_mut(&window) {
                    let tracked_ids: std::collections::HashSet<TmuxPaneId> =
                        win.panes.iter().map(|p| p.pane_id).collect();
                    if tracked_ids != layout_pane_ids {
                        panes_changed = true;
                    }
                    win.layout = Some(layout.clone());
                } else {
                    panes_changed = true;
                }
                if panes_changed && self.attached_once {
                    self.queue_sync_topology(follow_up_cmds, false);
                }
                out.push(MachineEvent::Notification(notif));
            }
            TmuxNotification::SubscriptionChanged {
                pane: Some(pane_id),
                ref value,
                ..
            } => {
                let (cwd, title) = value
                    .split_once(FIELD_SEP)
                    .map(|(c, t)| (unquote(c), t.to_owned()))
                    .unwrap_or_else(|| (String::new(), value.clone()));
                if let Some(pane) = self.panes.get_mut(&pane_id) {
                    pane.title = title.clone();
                    if !cwd.is_empty() {
                        pane.cwd = cwd.clone();
                    }
                }
                out.push(MachineEvent::PaneMetadata {
                    pane_id,
                    title,
                    cwd,
                });
            }
            TmuxNotification::Exit { reason, trailing } => {
                let reason = self.attach_error.take().or(reason);
                out.push(MachineEvent::Detached { reason, trailing });
            }
            other => {
                out.push(MachineEvent::Notification(other));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn machine_bootstraps_and_syncs_topology() {
        let mut machine = TmuxMachine::new(120, 40, 500, 5);
        // Nothing goes out before tmux has answered its own attach command.
        let events = machine.feed(b"%begin 1 1 0\n");
        assert!(
            events
                .iter()
                .all(|e| !matches!(e, MachineEvent::SendCommands(_)))
        );
        let events = machine.feed(b"%end 1 1 0\n");
        let Some(MachineEvent::SendCommands(init_cmds)) = events.first() else {
            panic!("expected bootstrap commands, got {events:?}");
        };
        assert!(init_cmds.contains("refresh-client -C 120x40"));
        assert!(init_cmds.contains("list-windows"));
        assert!(init_cmds.contains("list-panes"));

        let simulated = b"\
                          %session-changed $0 work\n\
                          %begin 1 2 1\n%end 1 2 1\n\
                          %begin 1 3 1\n%end 1 3 1\n\
                          %begin 1 4 1\n%end 1 4 1\n\
                          %begin 1 5 1\n\
                          @0|~|1|~|b25f,120x40,0,0,0|~|dev\n\
                          %end 1 5 1\n\
                          %begin 1 6 1\n\
                          @0|~|%0|~|1|~|120|~|40|~|/home/alex/project|~|nvim\n\
                          %end 1 6 1\n";
        let events = machine.feed(simulated);
        assert!(events.iter().any(|e| matches!(
            e,
            MachineEvent::TopologySynced {
                session_id: TmuxSessionId(0),
                session_name,
                windows,
            } if session_name == "work" && windows.len() == 1 && windows[0].panes.len() == 1
        )));
    }

    #[test]
    fn staged_topology_commits_only_when_both_commands_succeed() {
        let mut machine = TmuxMachine::new(120, 40, 500, 5);
        // Simulate initial commands: 4 setup commands
        let init_acks = b"%begin 1 1 0\n%end 1 1 0\n\
                          %begin 1 2 1\n%end 1 2 1\n\
                          %begin 1 3 1\n%end 1 3 1\n\
                          %begin 1 4 1\n%end 1 4 1\n";
        machine.feed(init_acks);

        // list-windows succeeds, but list-panes returns an error
        let partial_fail = b"%begin 1 5 1\n\
                             @0|~|1|~|b25f,120x40,0,0,0|~|dev\n\
                             %end 1 5 1\n\
                             %begin 1 6 1\n\
                             error reading panes\n\
                             %error 1 6 1\n";
        let events = machine.feed(partial_fail);
        // TopologySynced should NOT have been emitted because list-panes failed!
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, MachineEvent::TopologySynced { .. }))
        );
        assert!(machine.windows.is_empty());
        assert!(machine.panes.is_empty());
    }

    #[test]
    fn scrollback_capture_zero_disables_pane_capture() {
        let mut machine = TmuxMachine::new(120, 40, 0, 5);
        let bootstrap = b"%begin 1 1 0\n%end 1 1 0\n\
                          %begin 1 2 1\n%end 1 2 1\n\
                          %begin 1 3 1\n%end 1 3 1\n\
                          %begin 1 4 1\n%end 1 4 1\n\
                          %begin 1 5 1\n\
                          @0|~|1|~|b25f,120x40,0,0,0|~|dev\n\
                          %end 1 5 1\n\
                          %begin 1 6 1\n\
                          @0|~|%0|~|1|~|120|~|40|~|/home|~|shell\n\
                          %end 1 6 1\n";
        let events = machine.feed(bootstrap);
        // Ensure no SendCommands containing `capture-pane` was emitted
        for event in events {
            if let MachineEvent::SendCommands(cmd) = event {
                assert!(
                    !cmd.contains("capture-pane"),
                    "expected no capture-pane command, got {cmd}"
                );
            }
        }
    }

    #[test]
    fn layout_change_detects_removed_pane_and_triggers_sync() {
        let mut machine = TmuxMachine::new(120, 40, 0, 5);
        let bootstrap_2_panes = b"%begin 1 1 0\n%end 1 1 0\n\
                                  %begin 1 2 1\n%end 1 2 1\n\
                                  %begin 1 3 1\n%end 1 3 1\n\
                                  %begin 1 4 1\n%end 1 4 1\n\
                                  %begin 1 5 1\n\
                                  @0|~|1|~|a1b2,120x40,0,0{60x40,0,0,0,60x40,60,0,1}|~|dev\n\
                                  %end 1 5 1\n\
                                  %begin 1 6 1\n\
                                  @0|~|%0|~|1|~|60|~|40|~|/home|~|shell\n\
                                  @0|~|%1|~|0|~|60|~|40|~|/home|~|shell\n\
                                  %end 1 6 1\n";
        machine.feed(bootstrap_2_panes);
        assert_eq!(machine.panes.len(), 2);

        // tmux 3.5a closes pane 1: sends layout-change with only pane 0
        let layout_change_1_pane = b"%layout-change @0 b25f,120x40,0,0,0 \n";
        let events = machine.feed(layout_change_1_pane);
        // Follow-up commands should request topology sync because pane 1 was removed
        let has_sync = events.iter().any(|e| match e {
            MachineEvent::SendCommands(cmd) => {
                cmd.contains("list-windows") && cmd.contains("list-panes")
            }
            _ => false,
        });
        assert!(
            has_sync,
            "expected layout change to trigger topology sync for removed pane"
        );
    }

    #[test]
    fn layout_change_resize_only_does_not_trigger_sync() {
        let mut machine = TmuxMachine::new(120, 40, 0, 5);
        let bootstrap_1_pane = b"%begin 1 1 0\n%end 1 1 0\n\
                                %begin 1 2 1\n%end 1 2 1\n\
                                %begin 1 3 1\n%end 1 3 1\n\
                                %begin 1 4 1\n%end 1 4 1\n\
                                %begin 1 5 1\n\
                                @0|~|1|~|b25f,120x40,0,0,0|~|dev\n\
                                %end 1 5 1\n\
                                %begin 1 6 1\n\
                                @0|~|%0|~|1|~|120|~|40|~|/home|~|shell\n\
                                %end 1 6 1\n";
        machine.feed(bootstrap_1_pane);

        // Simple resize of pane 0 to 140x50
        let resize_pane_0 = b"%layout-change @0 b25f,140x50,0,0,0 \n";
        let events = machine.feed(resize_pane_0);
        let has_sync = events.iter().any(|e| match e {
            MachineEvent::SendCommands(cmd) => {
                cmd.contains("list-windows") || cmd.contains("list-panes")
            }
            _ => false,
        });
        assert!(
            !has_sync,
            "resize-only should update geometry without triggering full sync"
        );
        assert_eq!(machine.panes[&TmuxPaneId(0)].cols, 140);
        assert_eq!(machine.panes[&TmuxPaneId(0)].rows, 50);
    }

    #[test]
    fn failed_attach_sends_nothing_and_reports_the_error() {
        // What tmux 3.5a sends for `tmux -CC attach -t missing`.
        let mut machine = TmuxMachine::new(120, 40, 500, 5);
        let events = machine.feed(
            b"%begin 1795194397 343 0\r\ncan't find session: missing\r\n\
              %error 1795194397 343 0\r\n%exit\r\n\x1b\\",
        );
        assert!(
            events
                .iter()
                .all(|e| !matches!(e, MachineEvent::SendCommands(_))),
            "nothing may be sent to a shell: {events:?}"
        );
        assert!(events.iter().any(|e| matches!(
            e,
            MachineEvent::Detached { reason: Some(r), .. } if r == "can't find session: missing"
        )));
    }

    #[test]
    fn free_text_fields_keep_separators_and_spaces() {
        let mut machine = TmuxMachine::new(120, 40, 0, 5);
        let events = machine.feed(
            b"%begin 1 1 0\n%end 1 1 0\n\
              %begin 1 2 1\n%end 1 2 1\n\
              %begin 1 3 1\n%end 1 3 1\n\
              %begin 1 4 1\n%end 1 4 1\n\
              %begin 1 5 1\n\
              @0|~|1|~|b25f,120x40,0,0,0|~|my window|~|odd\n\
              %end 1 5 1\n\
              %begin 1 6 1\n\
              @0|~|%0|~|1|~|120|~|40|~|/home/a\\|~\\|b\\ c|~|vim: a|~|b.txt\n\
              %end 1 6 1\n",
        );
        let windows = events
            .iter()
            .find_map(|e| match e {
                MachineEvent::TopologySynced { windows, .. } => Some(windows),
                _ => None,
            })
            .expect("topology");
        assert_eq!(windows[0].name, "my window|~|odd");
        // tmux 3.5a's `#{q:…}` output for the directory `/home/a|~|b c`.
        assert_eq!(windows[0].panes[0].cwd, "/home/a|~|b c");
        assert_eq!(windows[0].panes[0].title, "vim: a|~|b.txt");
    }

    #[test]
    fn typed_commands_keep_replies_in_step_however_many_they_run() {
        let mut machine = TmuxMachine::new(120, 40, 0, 5);
        machine.feed(
            b"%begin 1 1 0\n%end 1 1 0\n\
              %begin 1 2 1\n%end 1 2 1\n\
              %begin 1 3 1\n%end 1 3 1\n\
              %begin 1 4 1\n%end 1 4 1\n\
              %begin 1 5 1\n%end 1 5 1\n\
              %begin 1 6 1\n%end 1 6 1\n",
        );
        // Sent unchanged: quoted `;` stays part of the argument.
        let sent = machine.queue_raw_command(
            r#"display -p "a;b" ; if-shell true { display -p x ; display -p y }"#,
        );
        let (command, marker_cmd) = sent.split_once('\n').expect("two lines");
        assert_eq!(
            command,
            r#"display -p "a;b" ; if-shell true { display -p x ; display -p y }"#
        );
        let marker = marker_cmd
            .strip_prefix("display-message -p ")
            .and_then(|m| m.strip_suffix('\n'))
            .expect("marker command");
        assert!(marker.starts_with("rooste-sync-") && marker.len() > "rooste-sync-1".len());
        machine.queue_sync_topology(&mut String::new(), false);
        // tmux 3.5a: one block each for display, if-shell, x and y, then the
        // marker. A line that only looks like a marker must not end the run.
        let events = machine.feed(
            format!(
                "%begin 1 7 1\na;b\n%end 1 7 1\n\
                 %begin 1 8 1\n%end 1 8 1\n\
                 %begin 1 9 1\nrooste-sync-1\n%end 1 9 1\n\
                 %begin 1 10 1\ny\n%end 1 10 1\n\
                 %begin 1 11 1\n{marker}\n%end 1 11 1\n\
                 %begin 1 12 1\n@0|~|1|~|b25f,120x40,0,0,0|~|dev\n%end 1 12 1\n\
                 %begin 1 13 1\n@0|~|%0|~|1|~|120|~|40|~|/home|~|sh\n%end 1 13 1\n"
            )
            .as_bytes(),
        );
        assert!(events.iter().any(|e| matches!(
            e,
            MachineEvent::TopologySynced { windows, .. } if windows.len() == 1 && windows[0].name == "dev"
        )));
    }

    #[test]
    fn markers_differ_between_connections() {
        let marker = |m: &mut TmuxMachine| m.queue_raw_command("list-sessions");
        assert_ne!(
            marker(&mut TmuxMachine::new(80, 24, 0, 5)),
            marker(&mut TmuxMachine::new(80, 24, 0, 5))
        );
    }
}
