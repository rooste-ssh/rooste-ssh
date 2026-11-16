//! Sans-I/O state machine tracking tmux sessions, windows, panes, and pending commands.

use std::collections::{BTreeMap, VecDeque};

use crate::ids::{TmuxPaneId, TmuxSessionId, TmuxWindowId};
use crate::layout::{LayoutNode, parse_layout};
use crate::parser::{ControlParser, TmuxEvent, TmuxNotification, TmuxResponse};
use crate::unescape::escape_tmux_arg;

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
                let trimmed = command.trim();
                if trimmed.is_empty() {
                    String::new()
                } else {
                    format!("{trimmed}\n")
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
    /// Control mode exited cleanly or with a reason.
    Detached { reason: Option<String> },
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
    scrollback_capture: usize,
    pause_after_secs: u16,
    attached_once: bool,
}

impl TmuxMachine {
    /// Create a new state machine and return the machine along with the initial
    /// bootstrap command sequence to send over the SSH transport.
    pub fn new(
        client_cols: u16,
        client_rows: u16,
        scrollback_capture: usize,
        pause_after_secs: u16,
    ) -> (Self, String) {
        let mut parser = ControlParser::new();
        parser.log_lines = true;
        let mut pending = VecDeque::new();
        // When `tmux -CC` starts, tmux emits an unsolicited `%begin..%end` for the
        // `new-session` or `attach-session` command itself before our commands run.
        pending.push_back(PendingCmd::InitialAttach);

        let mut machine = Self {
            parser,
            pending,
            session_id: TmuxSessionId(0),
            session_name: "tmux".to_owned(),
            windows: BTreeMap::new(),
            panes: BTreeMap::new(),
            scrollback_capture: scrollback_capture.clamp(100, 50_000),
            pause_after_secs: pause_after_secs.max(1),
            attached_once: false,
        };

        let mut init_cmds = String::new();
        machine.queue_command(
            PendingCmd::RefreshClientSize,
            &format!(
                "refresh-client -C {}x{}\n",
                client_cols.max(20),
                client_rows.max(5)
            ),
            &mut init_cmds,
        );
        machine.queue_command(
            PendingCmd::RefreshClientFlags,
            &format!(
                "refresh-client -f pause-after={}\n",
                machine.pause_after_secs
            ),
            &mut init_cmds,
        );
        machine.queue_command(
            PendingCmd::SubscribeFormats,
            "refresh-client -B \"rooste_meta:%*:#{pane_title}\\t#{pane_current_path}\"\n",
            &mut init_cmds,
        );
        machine.queue_sync_topology(&mut init_cmds, true);

        (machine, init_cmds)
    }

    fn queue_command(&mut self, kind: PendingCmd, raw: &str, out: &mut String) {
        self.pending.push_back(kind);
        out.push_str(raw);
    }

    /// Queue `list-windows` and `list-panes` to reconcile full session state.
    pub fn queue_sync_topology(&mut self, out: &mut String, finalize_attach: bool) {
        self.queue_command(
            PendingCmd::ListWindows,
            "list-windows -F \"#{window_id}\\t#{window_name}\\t#{window_active}\\t#{window_layout}\"\n",
            out,
        );
        self.queue_command(
            PendingCmd::ListPanes { finalize_attach },
            "list-panes -s -F \"#{window_id}\\t#{pane_id}\\t#{pane_active}\\t#{pane_width}\\t#{pane_height}\\t#{pane_title}\\t#{pane_current_path}\"\n",
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

        if !follow_up_cmds.is_empty() {
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
        let cmd = self.pending.pop_front().unwrap_or(PendingCmd::UserCommand);
        if resp.is_error {
            out.push(MachineEvent::ProtocolLog(format!(
                "tmux command error: {}",
                resp.lines.join(" | ")
            )));
            return;
        }
        match cmd {
            PendingCmd::ListWindows => {
                self.windows.clear();
                for line in &resp.lines {
                    let mut parts = line.splitn(4, '\t');
                    let Some(win_id) = parts.next().and_then(|s| s.parse::<TmuxWindowId>().ok())
                    else {
                        continue;
                    };
                    let name = parts.next().unwrap_or("shell").to_owned();
                    let active = parts.next() == Some("1");
                    let layout = parts.next().and_then(|s| parse_layout(s).ok());
                    self.windows.insert(
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
            }
            PendingCmd::ListPanes { finalize_attach } => {
                let previous_panes: std::collections::HashSet<TmuxPaneId> =
                    self.panes.keys().copied().collect();
                self.panes.clear();
                for win in self.windows.values_mut() {
                    win.panes.clear();
                }
                for line in &resp.lines {
                    let mut parts = line.splitn(7, '\t');
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
                    let title = parts.next().unwrap_or_default().to_owned();
                    let cwd = parts.next().unwrap_or_default().to_owned();

                    let snap = TmuxPaneSnapshot {
                        window_id,
                        pane_id,
                        active,
                        cols,
                        rows,
                        title,
                        cwd,
                    };
                    self.panes.insert(pane_id, snap.clone());
                    if let Some(win) = self.windows.get_mut(&window_id) {
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
            | PendingCmd::UserCommand => {}
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
            TmuxNotification::WindowClose { window } => {
                self.windows.remove(&window);
                self.panes.retain(|_, p| p.window_id != window);
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
                let mut unknown_pane = false;
                for (pane_id, geom) in layout.panes() {
                    if let Some(pane) = self.panes.get_mut(&pane_id) {
                        pane.cols = geom.cols;
                        pane.rows = geom.rows;
                    } else {
                        unknown_pane = true;
                    }
                }
                if let Some(win) = self.windows.get_mut(&window) {
                    win.layout = Some(layout.clone());
                } else {
                    unknown_pane = true;
                }
                if unknown_pane && self.attached_once {
                    self.queue_sync_topology(follow_up_cmds, false);
                }
                out.push(MachineEvent::Notification(notif));
            }
            TmuxNotification::SubscriptionChanged {
                pane: Some(pane_id),
                ref value,
                ..
            } => {
                let (title, cwd) = value
                    .split_once('\t')
                    .map(|(t, c)| (t.to_owned(), c.to_owned()))
                    .unwrap_or_else(|| (value.clone(), String::new()));
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
            TmuxNotification::Exit { reason } => {
                out.push(MachineEvent::Detached { reason });
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
        let (mut machine, init_cmds) = TmuxMachine::new(120, 40, 500, 5);
        assert!(init_cmds.contains("refresh-client -C 120x40"));
        assert!(init_cmds.contains("list-windows"));
        assert!(init_cmds.contains("list-panes"));

        let simulated = b"%begin 1 1 0\n%end 1 1 0\n\
                          %session-changed $0 work\n\
                          %begin 1 2 1\n%end 1 2 1\n\
                          %begin 1 3 1\n%end 1 3 1\n\
                          %begin 1 4 1\n%end 1 4 1\n\
                          %begin 1 5 1\n\
                          @0\tdev\t1\tb25f,120x40,0,0,0\n\
                          %end 1 5 1\n\
                          %begin 1 6 1\n\
                          @0\t%0\t1\t120\t40\tnvim\t/home/alex/project\n\
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
}
