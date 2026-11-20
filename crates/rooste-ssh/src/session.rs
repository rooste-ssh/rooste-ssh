//! A connection running on the tokio runtime, talking to the UI thread
//! through channels so a slow network never blocks rendering.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;

use futures::future::BoxFuture;
use rooste_core::{Destination, Protocol, SerialSettings, Tunnel, TunnelKind};
use rooste_protocols::Transport;
use rooste_protocols::ssh::{
    AgentSource, Auth, AuthMethod, Forward, HostKeyPolicy, HostKeyPrompt, InteractiveStep,
    PtyRequest, SshClient, SshConfig, SshError, UnknownHostKey, X11Forwarding, connect,
    connect_via, load_private_key,
};
use rooste_tmux::{
    DcsDetector, MachineEvent, TmuxClientAction, TmuxMachine, TmuxNotification, TmuxPaneId,
    TmuxSessionId, TmuxWindowSnapshot, encode_paste_hex, encode_send_keys_hex,
};
use tokio::sync::{mpsc, oneshot};

/// Everything needed to open a session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConnectRequest {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: Option<String>,
    pub key_file: Option<PathBuf>,
    pub key_passphrase: Option<String>,
    pub use_agent: bool,
    pub allow_legacy: bool,
    /// Override for tests; `None` uses `~/.ssh/known_hosts`.
    pub known_hosts: Option<PathBuf>,
    pub cols: u16,
    pub rows: u16,
    /// Ask for a password when no other method works (as `ssh` does).
    pub prompt_for_password: bool,
    /// Jump hosts, as OpenSSH `ProxyJump`.
    pub proxy_jump: Option<String>,
    pub forward_agent: bool,
    /// Forward X11 to the local display.
    pub forward_x11: bool,
    /// Agent used for login and forwarding.
    pub agent: AgentSource,
    pub tunnels: Vec<Tunnel>,
    pub protocol: Protocol,
    /// Line settings when `protocol` is serial (`host` holds the port).
    pub serial: SerialSettings,
    /// Optional `tmux -CC` startup command to run immediately upon connection.
    pub tmux_command: Option<String>,
    /// Scrollback lines captured per pane when attaching via `tmux -CC`.
    pub tmux_scrollback_capture: usize,
    /// Flow-control pause threshold in seconds for `tmux -CC`.
    pub tmux_pause_after_secs: u16,
}

/// How a session ended unsuccessfully.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub message: String,
    /// The server would be reachable with legacy algorithms enabled.
    pub legacy_hint: bool,
    /// An established connection dropped (worth reconnecting).
    pub lost: bool,
}

impl Failure {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            legacy_hint: false,
            lost: false,
        }
    }

    fn lost(message: impl Into<String>) -> Self {
        Self {
            lost: true,
            ..Self::new(message)
        }
    }
}

impl From<SshError> for Failure {
    fn from(error: SshError) -> Self {
        let legacy_hint = matches!(
            error,
            SshError::NoCommonAlgorithm {
                legacy_would_help: true,
                ..
            }
        );
        Self {
            message: describe(error),
            legacy_hint,
            lost: false,
        }
    }
}

impl From<String> for Failure {
    fn from(message: String) -> Self {
        Self::new(message)
    }
}

/// State of one saved tunnel after the session started it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelStatus {
    pub description: String,
    /// `Err` explains why the tunnel could not start.
    pub result: Result<String, String>,
}

/// What a prompt asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptKind {
    Password {
        user: String,
        host: String,
    },
    Passphrase {
        key_file: PathBuf,
    },
    /// A question from the server in a keyboard-interactive login, such as
    /// a password or a one-time code.
    Interactive {
        user: String,
        host: String,
        /// The server's title and instructions for this step (may be empty).
        name: String,
        instructions: String,
        prompt: String,
        /// Whether the answer may be shown as typed.
        echo: bool,
    },
    /// A question from a running script.
    Script {
        script: String,
        question: String,
        /// Hide the answer as it is typed (passwords).
        hidden: bool,
    },
}

const PROMPT_ATTEMPTS: usize = 3;

impl PromptKind {
    /// A question from a script.
    pub fn script(script: &str, question: &str, hidden: bool) -> Self {
        PromptKind::Script {
            script: script.to_owned(),
            question: question.to_owned(),
            hidden,
        }
    }
}

/// From the session to the UI.
#[derive(Debug)]
pub enum SessionEvent {
    Status(String),
    /// The server's host key is unknown; answer `true` to trust it.
    HostKey {
        key: UnknownHostKey,
        reply: oneshot::Sender<bool>,
    },
    /// A secret is needed; answer `None` to cancel.
    Prompt {
        kind: PromptKind,
        /// The previous attempt was rejected.
        retry: bool,
        reply: oneshot::Sender<Option<String>>,
    },
    /// Login succeeded with this password (so the UI may save it).
    PasswordAccepted(String),
    Connected,
    /// The SFTP session for the file panel is ready.
    Files(crate::files::FilesHandle),
    FilesFailed(String),
    Tunnels(Vec<TunnelStatus>),
    Output(Vec<u8>),
    /// Remote session entered `tmux -CC` control mode and synced its window/pane topology.
    TmuxTopologySynced {
        session_id: TmuxSessionId,
        session_name: String,
        windows: Vec<TmuxWindowSnapshot>,
    },
    /// Decoded VT output for a specific tmux pane (`%output` or `capture-pane`).
    TmuxPaneOutput {
        pane_id: TmuxPaneId,
        bytes: Vec<u8>,
    },
    /// Incremental notification from `tmux -CC` (`%window-add`, `%layout-change`, `%pause`, etc.).
    TmuxNotification(TmuxNotification),
    /// Updated pane title and working directory from format subscription.
    TmuxPaneMetadata {
        pane_id: TmuxPaneId,
        title: String,
        cwd: String,
    },
    /// Diagnostic line from the `tmux -CC` protocol stream.
    TmuxProtocolLog(String),
    /// `tmux -CC` control mode exited cleanly or with a reason.
    TmuxDetached {
        reason: Option<String>,
    },
    Closed {
        exit_status: Option<u32>,
    },
    Failed(Failure),
}

/// From the UI to the session.
#[derive(Debug)]
pub enum SessionCommand {
    Input(Vec<u8>),
    /// Start SFTP for the file panel.
    OpenFiles,
    Resize(u16, u16),
    /// Send raw keyboard/mouse bytes to a specific `tmux -CC` pane (`send-keys -H`).
    TmuxPaneInput {
        pane_id: TmuxPaneId,
        bytes: Vec<u8>,
    },
    /// Paste text into a specific `tmux -CC` pane.
    TmuxPanePaste {
        pane_id: TmuxPaneId,
        text: String,
        bracketed: bool,
    },
    /// Update the `tmux -CC` client's maximum window dimensions (`refresh-client -C`).
    TmuxResizeClient {
        cols: u16,
        rows: u16,
    },
    /// Resize an individual `tmux -CC` pane (`resize-pane -t %id -x <cols> -y <rows>`).
    TmuxResizePane {
        pane_id: TmuxPaneId,
        cols: u16,
        rows: u16,
    },
    /// Execute a structured `tmux -CC` client action (new window, split, kill, etc.).
    TmuxAction(TmuxClientAction),
    /// Detach cleanly from `tmux -CC` (`force: false`) or force-exit (`force: true`).
    ExitTmuxControlMode {
        force: bool,
    },
    Close,
}

/// Cloneable sender handle allowing child `TmuxPane` tabs to send commands to their gateway session.
#[derive(Debug, Clone)]
pub struct SessionSender {
    commands: mpsc::UnboundedSender<SessionCommand>,
}

impl SessionSender {
    pub fn send(&self, command: SessionCommand) {
        let _ = self.commands.send(command);
    }
}

/// Handle owned by the UI.
#[derive(Debug)]
pub struct Session {
    commands: mpsc::UnboundedSender<SessionCommand>,
    events: std_mpsc::Receiver<SessionEvent>,
}

impl Session {
    /// Start connecting. `wake` is called whenever an event is queued, so
    /// the UI can repaint.
    pub fn spawn(
        runtime: &tokio::runtime::Handle,
        request: ConnectRequest,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (event_tx, events) = std_mpsc::channel();
        let sink = EventSink {
            tx: event_tx,
            wake: Arc::new(wake),
        };
        runtime.spawn(async move {
            let done = match run(request, command_rx, &sink).await {
                Ok(exit_status) => SessionEvent::Closed { exit_status },
                Err(failure) => SessionEvent::Failed(failure),
            };
            sink.send(done);
        });
        Self { commands, events }
    }

    pub fn send(&self, command: SessionCommand) {
        let _ = self.commands.send(command);
    }

    /// Obtain a cloneable command sender for child tmux panes.
    pub fn sender(&self) -> SessionSender {
        SessionSender {
            commands: self.commands.clone(),
        }
    }

    /// Events queued since the last call.
    pub fn poll(&self) -> Vec<SessionEvent> {
        self.events.try_iter().collect()
    }
}

#[derive(Clone)]
struct EventSink {
    tx: std_mpsc::Sender<SessionEvent>,
    wake: Arc<dyn Fn() + Send + Sync>,
}

impl EventSink {
    fn send(&self, event: SessionEvent) {
        if self.tx.send(event).is_ok() {
            (self.wake)();
        }
    }
}

struct UiPrompt(EventSink);

impl HostKeyPrompt for UiPrompt {
    fn confirm_unknown(&self, key: UnknownHostKey) -> BoxFuture<'static, bool> {
        let (reply, answer) = oneshot::channel();
        self.0.send(SessionEvent::HostKey { key, reply });
        Box::pin(async move { answer.await.unwrap_or(false) })
    }
}

async fn run(
    request: ConnectRequest,
    commands: mpsc::UnboundedReceiver<SessionCommand>,
    sink: &EventSink,
) -> Result<Option<u32>, Failure> {
    use std::time::Duration;
    let timeout = Duration::from_secs(15);
    let term = "xterm-256color";
    let (cols, rows) = (request.cols.max(1), request.rows.max(1));
    let open_error = |e: std::io::Error| Failure::new(format!("Could not connect: {e}"));
    match request.protocol {
        Protocol::Ssh => run_ssh(request, commands, sink).await,
        Protocol::Telnet => {
            sink.send(SessionEvent::Status(format!(
                "Connecting to {}:{}…",
                request.host, request.port
            )));
            let transport = rooste_protocols::telnet::TelnetTransport::connect(
                &request.host,
                request.port,
                timeout,
                term,
                cols,
                rows,
            )
            .await
            .map_err(open_error)?;
            pump(transport, commands, sink, &request).await
        }
        Protocol::Raw => {
            sink.send(SessionEvent::Status(format!(
                "Connecting to {}:{}…",
                request.host, request.port
            )));
            let transport =
                rooste_protocols::raw::RawTransport::connect(&request.host, request.port, timeout)
                    .await
                    .map_err(open_error)?;
            pump(transport, commands, sink, &request).await
        }
        Protocol::Serial => {
            let options = serial_options(&request.serial);
            sink.send(SessionEvent::Status(format!(
                "Opening {} ({})…",
                request.host,
                request.serial.describe()
            )));
            let transport = rooste_protocols::serial::SerialTransport::open(&request.host, options)
                .map_err(|e| Failure::new(format!("Could not open the serial port: {e}")))?;
            pump(transport, commands, sink, &request).await
        }
    }
}

fn serial_options(settings: &SerialSettings) -> rooste_protocols::serial::SerialOptions {
    use rooste_protocols::serial::{SerialFlow, SerialOptions, SerialParity};
    SerialOptions {
        baud: settings.baud,
        data_bits: settings.data_bits,
        parity: match settings.parity {
            rooste_core::Parity::None => SerialParity::None,
            rooste_core::Parity::Odd => SerialParity::Odd,
            rooste_core::Parity::Even => SerialParity::Even,
        },
        stop_bits: settings.stop_bits,
        flow: match settings.flow_control {
            rooste_core::FlowControl::None => SerialFlow::None,
            rooste_core::FlowControl::XonXoff => SerialFlow::XonXoff,
            rooste_core::FlowControl::RtsCts => SerialFlow::RtsCts,
        },
    }
}

/// Forward events emitted by `TmuxMachine` to the UI sink and write any
/// follow-up protocol commands to `transport`. Returns `true` if `tmux -CC` detached.
async fn dispatch_tmux_events<T: Transport>(
    events: Vec<MachineEvent>,
    transport: &mut T,
    sink: &EventSink,
) -> Result<bool, Failure> {
    let mut detached = false;
    for ev in events {
        match ev {
            MachineEvent::SendCommands(cmds) => {
                if let Err(e) = transport.write(cmds.as_bytes()).await {
                    return Err(Failure::lost(format!("Connection lost: {e}")));
                }
            }
            MachineEvent::TopologySynced {
                session_id,
                session_name,
                windows,
            } => {
                sink.send(SessionEvent::TmuxTopologySynced {
                    session_id,
                    session_name,
                    windows,
                });
            }
            MachineEvent::PaneOutput { pane_id, bytes } => {
                sink.send(SessionEvent::TmuxPaneOutput { pane_id, bytes });
            }
            MachineEvent::Notification(notif) => {
                sink.send(SessionEvent::TmuxNotification(notif));
            }
            MachineEvent::PaneMetadata {
                pane_id,
                title,
                cwd,
            } => {
                sink.send(SessionEvent::TmuxPaneMetadata {
                    pane_id,
                    title,
                    cwd,
                });
            }
            MachineEvent::ProtocolLog(line) => {
                sink.send(SessionEvent::TmuxProtocolLog(line));
            }
            MachineEvent::Detached { reason, trailing } => {
                sink.send(SessionEvent::TmuxDetached { reason });
                if !trailing.is_empty() {
                    sink.send(SessionEvent::Output(trailing));
                }
                detached = true;
            }
        }
    }
    Ok(detached)
}

async fn write_raw_text<T: Transport>(transport: &mut T, text: &str) -> Result<(), Failure> {
    if !text.is_empty()
        && let Err(e) = transport.write(text.as_bytes()).await
    {
        return Err(Failure::lost(format!("Connection lost: {e}")));
    }
    Ok(())
}

async fn handle_tmux_command<T: Transport>(
    command: SessionCommand,
    tmux: &mut Option<TmuxMachine>,
    dcs: &mut DcsDetector,
    transport: &mut T,
    client_size: &mut (u16, u16),
    sink: &EventSink,
) -> Result<bool, Failure> {
    match command {
        SessionCommand::Input(data) => {
            if tmux.is_some() {
                // Ignore raw terminal input while in tmux control mode so control streams don't get corrupted.
                return Ok(false);
            }
            if let Err(e) = transport.write(&data).await {
                return Err(Failure::lost(format!("Connection lost: {e}")));
            }
        }
        SessionCommand::Resize(cols, rows) => {
            *client_size = (cols.max(1), rows.max(1));
            if let Some(machine) = tmux.as_mut().filter(|m| !m.is_ready()) {
                machine.set_client_size(client_size.0, client_size.1);
            } else if let Some(machine) = tmux.as_mut() {
                machine.register_user_command();
                write_raw_text(
                    transport,
                    &format!("refresh-client -C {}x{}\n", client_size.0, client_size.1),
                )
                .await?;
            } else {
                let _ = transport.resize(cols, rows).await;
            }
        }
        SessionCommand::TmuxPaneInput { pane_id, bytes } => {
            if let Some(machine) = tmux.as_mut().filter(|m| m.is_ready()) {
                let cmd = encode_send_keys_hex(pane_id, &bytes);
                if !cmd.is_empty() {
                    machine.register_user_command();
                    write_raw_text(transport, &cmd).await?;
                }
            }
        }
        SessionCommand::TmuxPanePaste {
            pane_id,
            text,
            bracketed,
        } => {
            if let Some(machine) = tmux.as_mut().filter(|m| m.is_ready()) {
                let cmd = encode_paste_hex(pane_id, &text, bracketed);
                if !cmd.is_empty() {
                    machine.register_user_command();
                    write_raw_text(transport, &cmd).await?;
                }
            }
        }
        SessionCommand::TmuxResizeClient { cols, rows } => {
            *client_size = (cols.max(20), rows.max(5));
            if let Some(machine) = tmux.as_mut().filter(|m| !m.is_ready()) {
                machine.set_client_size(client_size.0, client_size.1);
            } else if let Some(machine) = tmux.as_mut().filter(|m| m.is_ready()) {
                machine.register_user_command();
                write_raw_text(
                    transport,
                    &format!("refresh-client -C {}x{}\n", client_size.0, client_size.1),
                )
                .await?;
            }
        }
        SessionCommand::TmuxResizePane {
            pane_id,
            cols,
            rows,
        } => {
            if let Some(machine) = tmux.as_mut().filter(|m| m.is_ready()) {
                machine.register_user_command();
                write_raw_text(
                    transport,
                    &format!(
                        "resize-pane -t {} -x {} -y {}\n",
                        pane_id,
                        cols.max(2),
                        rows.max(2)
                    ),
                )
                .await?;
            }
        }
        SessionCommand::TmuxAction(action) => {
            if let Some(machine) = tmux.as_mut().filter(|m| m.is_ready()) {
                match action {
                    TmuxClientAction::RawCommand { command } => {
                        let cmd = machine.queue_raw_command(&command);
                        write_raw_text(transport, &cmd).await?;
                    }
                    action => {
                        let cmd = action.to_command_string();
                        if !cmd.is_empty() {
                            machine.register_user_command();
                            write_raw_text(transport, &cmd).await?;
                        }
                    }
                }
            }
        }
        SessionCommand::ExitTmuxControlMode { force } => {
            if tmux.is_some() {
                if force {
                    *tmux = None;
                    dcs.reset();
                    write_raw_text(transport, "\nstty sane\n").await?;
                    sink.send(SessionEvent::TmuxDetached {
                        reason: Some("force-exited tmux control mode".to_owned()),
                    });
                } else {
                    // In `tmux -CC`, sending an empty line or `detach-client` cleanly detaches.
                    write_raw_text(transport, "detach-client\n").await?;
                }
            }
        }
        SessionCommand::OpenFiles => {
            sink.send(SessionEvent::FilesFailed(
                "File transfer needs an SSH connection.".into(),
            ));
        }
        SessionCommand::Close => {
            let _ = transport.close().await;
            return Ok(true);
        }
    }
    Ok(false)
}

/// Run a non-SSH session: copy data both ways until it ends, also supporting `tmux -CC`.
async fn pump<T: Transport>(
    mut transport: T,
    mut commands: mpsc::UnboundedReceiver<SessionCommand>,
    sink: &EventSink,
    request: &ConnectRequest,
) -> Result<Option<u32>, Failure> {
    sink.send(SessionEvent::Connected);
    let mut buf = vec![0u8; 64 * 1024];
    let mut dcs = DcsDetector::new();
    let mut tmux: Option<TmuxMachine> = None;
    let mut client_size = (request.cols.max(80), request.rows.max(24));
    let scrollback_capture = request.tmux_scrollback_capture;
    let pause_after = if request.tmux_pause_after_secs == 0 {
        5
    } else {
        request.tmux_pause_after_secs
    };

    loop {
        tokio::select! {
            read = transport.read(&mut buf) => match read {
                Ok(0) => return Ok(None),
                Ok(n) => {
                    let chunk = &buf[..n];
                    if let Some(machine) = tmux.as_mut() {
                        let events = machine.feed(chunk);
                        if dispatch_tmux_events(events, &mut transport, sink).await? {
                            tmux = None;
                            dcs.reset();
                        }
                    } else if let Some((before, after)) = dcs.scan(chunk) {
                        if !before.is_empty() {
                            sink.send(SessionEvent::Output(before.to_vec()));
                        }
                        let mut machine = TmuxMachine::new(
                            client_size.0,
                            client_size.1,
                            scrollback_capture,
                            pause_after,
                        );
                        let events = machine.feed(after);
                        if !dispatch_tmux_events(events, &mut transport, sink).await? {
                            tmux = Some(machine);
                        }
                    } else {
                        sink.send(SessionEvent::Output(chunk.to_vec()));
                    }
                }
                Err(e) => return Err(Failure::lost(format!("Connection lost: {e}"))),
            },
            command = commands.recv() => match command {
                Some(cmd) => {
                    if handle_tmux_command(
                        cmd,
                        &mut tmux,
                        &mut dcs,
                        &mut transport,
                        &mut client_size,
                        sink,
                    )
                    .await?
                    {
                        return Ok(None);
                    }
                }
                None => {
                    let _ = transport.close().await;
                    return Ok(None);
                }
            },
        }
    }
}

async fn run_ssh(
    request: ConnectRequest,
    mut commands: mpsc::UnboundedReceiver<SessionCommand>,
    sink: &EventSink,
) -> Result<Option<u32>, Failure> {
    let config_for = |host: &str, port: u16| {
        let mut config = SshConfig::new(host, port);
        config.allow_legacy_algorithms = request.allow_legacy;
        config.host_key_policy = HostKeyPolicy::Ask(Arc::new(UiPrompt(sink.clone())));
        if let Some(path) = &request.known_hosts {
            config.known_hosts = path.clone();
        }
        config
    };

    // Each jump host is reached through the previous one.
    let hops = match request.proxy_jump.as_deref() {
        Some(jump) => Destination::parse_list(jump).map_err(|e| format!("Jump host: {e}"))?,
        None => Vec::new(),
    };
    let mut previous: Option<SshClient> = None;
    for hop in hops {
        let user = hop.user.clone().unwrap_or_else(|| request.user.clone());
        let port = hop.port.unwrap_or(22);
        sink.send(SessionEvent::Status(format!(
            "Connecting to jump host {}:{port}…",
            hop.host
        )));
        let config = config_for(&hop.host, port);
        let mut client = match previous.take() {
            None => connect(&config).await?,
            Some(via) => connect_via(via, &config).await?,
        };
        let login = Login {
            user: &user,
            host: &hop.host,
            password: None,
            target: false,
        };
        if !authenticate(&mut client, &login, &request, sink).await? {
            return Err(format!("Authentication failed for jump host {user}@{}", hop.host).into());
        }
        previous = Some(client);
    }

    sink.send(SessionEvent::Status(format!(
        "Connecting to {}:{}…",
        request.host, request.port
    )));
    let config = config_for(&request.host, request.port);
    let mut client = match previous {
        None => connect(&config).await?,
        Some(via) => connect_via(via, &config).await?,
    };
    sink.send(SessionEvent::Status("Authenticating…".into()));
    let login = Login {
        user: &request.user,
        host: &request.host,
        password: request.password.as_deref(),
        target: true,
    };
    if !authenticate(&mut client, &login, &request, sink).await? {
        return Err(format!(
            "Authentication failed for {}@{}",
            request.user, request.host
        )
        .into());
    }

    let (_forwards, statuses) = start_tunnels(&client, &request.tunnels).await;
    if !statuses.is_empty() {
        sink.send(SessionEvent::Tunnels(statuses));
    }

    let pty = PtyRequest {
        cols: request.cols.max(1),
        rows: request.rows.max(1),
        ..PtyRequest::default()
    };
    let agent = request.forward_agent.then(|| request.agent.clone());
    if request.forward_x11 {
        match X11Forwarding::from_environment() {
            Ok(forwarding) => client.set_x11_forwarding(forwarding),
            Err(e) => sink.send(SessionEvent::Output(
                format!("\x1b[33mX11 forwarding is off: {e}\x1b[0m\r\n").into_bytes(),
            )),
        }
    }
    let mut shell = client.open_shell_with(&pty, agent).await?;
    sink.send(SessionEvent::Connected);

    if let Some(tmux_cmd) = &request.tmux_command
        && !tmux_cmd.trim().is_empty()
    {
        let _ = shell.write(tmux_cmd.as_bytes()).await;
    }

    let mut buf = vec![0u8; 64 * 1024];
    let mut dcs = DcsDetector::new();
    let mut tmux: Option<TmuxMachine> = None;
    let mut client_size = (request.cols.max(80), request.rows.max(24));
    let scrollback_capture = request.tmux_scrollback_capture;
    let pause_after = if request.tmux_pause_after_secs == 0 {
        5
    } else {
        request.tmux_pause_after_secs
    };

    loop {
        tokio::select! {
            read = shell.read(&mut buf) => match read {
                Ok(0) => break,
                Ok(n) => {
                    let chunk = &buf[..n];
                    if let Some(machine) = tmux.as_mut() {
                        let events = machine.feed(chunk);
                        if dispatch_tmux_events(events, &mut shell, sink).await? {
                            tmux = None;
                            dcs.reset();
                        }
                    } else if let Some((before, after)) = dcs.scan(chunk) {
                        if !before.is_empty() {
                            sink.send(SessionEvent::Output(before.to_vec()));
                        }
                        let mut machine = TmuxMachine::new(
                            client_size.0,
                            client_size.1,
                            scrollback_capture,
                            pause_after,
                        );
                        let events = machine.feed(after);
                        if !dispatch_tmux_events(events, &mut shell, sink).await? {
                            tmux = Some(machine);
                        }
                    } else {
                        sink.send(SessionEvent::Output(chunk.to_vec()));
                    }
                }
                Err(e) => return Err(Failure::lost(format!("Connection lost: {e}"))),
            },
            command = commands.recv() => match command {
                Some(SessionCommand::OpenFiles) => match rooste_sftp::Sftp::open(&client).await {
                    Ok(sftp) => {
                        let home = sftp.home().await.unwrap_or_else(|_| "/".into());
                        sink.send(SessionEvent::Files(crate::files::spawn(sftp, home, sink.wake.clone())));
                    }
                    Err(e) => sink.send(SessionEvent::FilesFailed(format!("SFTP is not available: {e}"))),
                },
                Some(SessionCommand::Close) | None => {
                    let _ = shell.close().await;
                    let _ = client.disconnect().await;
                    return Ok(shell.exit_status());
                }
                Some(other_cmd) => {
                    if handle_tmux_command(
                        other_cmd,
                        &mut tmux,
                        &mut dcs,
                        &mut shell,
                        &mut client_size,
                        sink,
                    )
                    .await?
                    {
                        let _ = client.disconnect().await;
                        return Ok(shell.exit_status());
                    }
                }
            },
        }
    }
    let exit_status = shell.exit_status();
    // The channel closed without an exit status because the connection
    // itself went away.
    if exit_status.is_none() && client.is_closed() {
        return Err(Failure::lost("Connection lost"));
    }
    let _ = client.disconnect().await;
    Ok(exit_status)
}

/// Start saved tunnels, reporting each one's outcome. The forwards stop
/// when the returned values are dropped.
async fn start_tunnels(
    client: &SshClient,
    tunnels: &[Tunnel],
) -> (Vec<Forward>, Vec<TunnelStatus>) {
    let mut forwards = Vec::new();
    let mut statuses = Vec::new();
    for tunnel in tunnels {
        let result = async {
            let (listen_host, listen_port) = tunnel.listen_addr()?;
            let bind = if listen_host.contains(':') {
                format!("[{listen_host}]:{listen_port}")
            } else {
                format!("{listen_host}:{listen_port}")
            };
            let forward = match tunnel.kind {
                TunnelKind::Local => {
                    let (host, port) = tunnel.target_addr()?;
                    client.forward_local(&bind, &host, port).await
                }
                TunnelKind::Dynamic => client.forward_dynamic(&bind).await,
                TunnelKind::Remote => {
                    let (host, port) = tunnel.target_addr()?;
                    client
                        .forward_remote(&listen_host, listen_port, &host, port)
                        .await
                }
            }
            .map_err(describe)?;
            Ok::<_, String>(forward)
        }
        .await;
        statuses.push(TunnelStatus {
            description: tunnel.describe(),
            result: result
                .as_ref()
                .map(|f| format!("listening on {}", f.listen))
                .map_err(Clone::clone),
        });
        if let Ok(forward) = result {
            forwards.push(forward);
        }
    }
    (forwards, statuses)
}

/// Who to log in as on one hop.
struct Login<'a> {
    user: &'a str,
    host: &'a str,
    /// Saved or entered password to try first.
    password: Option<&'a str>,
    /// The destination itself (prompted passwords may be saved).
    target: bool,
}

async fn authenticate(
    client: &mut SshClient,
    login: &Login<'_>,
    request: &ConnectRequest,
    sink: &EventSink,
) -> Result<bool, String> {
    let user = login.user;
    if request.use_agent {
        match client
            .authenticate(user, &Auth::Agent(request.agent.clone()))
            .await
        {
            Ok(true) => return Ok(true),
            Ok(false) | Err(SshError::Agent(_)) => {}
            Err(e) => return Err(describe(e)),
        }
    }
    if let Some(path) = &request.key_file
        && let Some(key) = load_key(path, request.key_passphrase.as_deref(), sink).await?
        && client
            .authenticate(user, &Auth::PublicKey(Arc::new(key)))
            .await
            .map_err(describe)?
    {
        return Ok(true);
    }

    // Like OpenSSH, prefer keyboard-interactive (which also covers one-time
    // codes) over plain passwords when the server offers both.
    let Some(methods) = client.auth_methods(user).await.map_err(describe)? else {
        return Ok(true);
    };
    if methods.contains(&AuthMethod::KeyboardInteractive) {
        match keyboard_interactive(client, login, request, sink).await? {
            Some(true) => return Ok(true),
            Some(false) => {}
            None => return Err("Login cancelled".into()),
        }
    }
    if !methods.contains(&AuthMethod::Password) {
        return Ok(false);
    }

    let mut retry = false;
    if let Some(password) = login.password {
        if try_password(client, user, password).await? {
            return Ok(true);
        }
        retry = true;
    }
    if !request.prompt_for_password {
        return Ok(false);
    }
    for _ in 0..PROMPT_ATTEMPTS {
        let kind = PromptKind::Password {
            user: user.to_owned(),
            host: login.host.to_owned(),
        };
        let Some(password) = prompt(sink, kind, retry).await else {
            return Err("Login cancelled".into());
        };
        if try_password(client, user, &password).await? {
            if login.target {
                sink.send(SessionEvent::PasswordAccepted(password));
            }
            return Ok(true);
        }
        retry = true;
    }
    Ok(false)
}

/// Log in by answering the server's questions. A saved password answers
/// the first hidden question that asks for a password; everything else is
/// asked in the UI. `None` if the user cancels.
async fn keyboard_interactive(
    client: &mut SshClient,
    login: &Login<'_>,
    request: &ConnectRequest,
    sink: &EventSink,
) -> Result<Option<bool>, String> {
    let user = login.user;
    let mut saved = login.password;
    for attempt in 0..PROMPT_ATTEMPTS {
        let mut retry = attempt > 0;
        let mut typed_password = None;
        let mut step = client
            .keyboard_interactive_start(user)
            .await
            .map_err(describe)?;
        // Servers may ask several rounds of questions; don't loop forever.
        for _ in 0..10 {
            let (name, instructions, prompts) = match step {
                InteractiveStep::Prompts {
                    name,
                    instructions,
                    prompts,
                } => (name, instructions, prompts),
                done => {
                    step = done;
                    break;
                }
            };
            let mut answers = Vec::with_capacity(prompts.len());
            for question in prompts {
                let asks_password =
                    !question.echo && question.prompt.to_lowercase().contains("password");
                if asks_password && let Some(password) = saved.take() {
                    answers.push(password.to_owned());
                    continue;
                }
                if !request.prompt_for_password {
                    return Ok(Some(false));
                }
                let kind = PromptKind::Interactive {
                    user: user.to_owned(),
                    host: login.host.to_owned(),
                    name: name.clone(),
                    instructions: instructions.clone(),
                    prompt: question.prompt.trim_end().to_owned(),
                    echo: question.echo,
                };
                let Some(answer) = prompt(sink, kind, retry).await else {
                    return Ok(None);
                };
                retry = false;
                if asks_password {
                    typed_password = Some(answer.clone());
                }
                answers.push(answer);
            }
            step = client
                .keyboard_interactive_respond(answers)
                .await
                .map_err(describe)?;
        }
        if step == InteractiveStep::Success {
            if let (true, Some(password)) = (login.target, typed_password) {
                sink.send(SessionEvent::PasswordAccepted(password));
            }
            return Ok(Some(true));
        }
    }
    Ok(Some(false))
}

async fn try_password(
    client: &mut rooste_protocols::ssh::SshClient,
    user: &str,
    password: &str,
) -> Result<bool, String> {
    client
        .authenticate(user, &Auth::password(password))
        .await
        .map_err(describe)
}

/// Load a key, asking for its passphrase if it is encrypted and none was
/// given. `None` if the user cancels.
async fn load_key(
    path: &std::path::Path,
    passphrase: Option<&str>,
    sink: &EventSink,
) -> Result<Option<rooste_protocols::ssh::PrivateKey>, String> {
    match load_private_key(path, passphrase) {
        Ok(key) => return Ok(Some(key)),
        Err(SshError::KeyEncrypted(_)) => {}
        Err(e) if passphrase.is_none() => return Err(describe(e)),
        Err(_) => {}
    }
    let mut retry = passphrase.is_some();
    for _ in 0..PROMPT_ATTEMPTS {
        let kind = PromptKind::Passphrase {
            key_file: path.to_owned(),
        };
        let Some(passphrase) = prompt(sink, kind, retry).await else {
            return Ok(None);
        };
        if let Ok(key) = load_private_key(path, Some(&passphrase)) {
            return Ok(Some(key));
        }
        retry = true;
    }
    Ok(None)
}

async fn prompt(sink: &EventSink, kind: PromptKind, retry: bool) -> Option<String> {
    let (reply, answer) = oneshot::channel();
    sink.send(SessionEvent::Prompt { kind, retry, reply });
    answer.await.ok().flatten()
}

/// A user-facing message for an error, including its causes.
pub fn describe(error: SshError) -> String {
    use std::error::Error;
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        // Some errors already include their cause's text.
        let text = cause.to_string();
        if !message.contains(&text) {
            message.push_str(": ");
            message.push_str(&text);
        }
        source = cause.source();
    }
    if let SshError::NoCommonAlgorithm {
        legacy_would_help: true,
        ..
    } = error
    {
        message.push_str(". This server only supports outdated algorithms.");
    }
    message
}
