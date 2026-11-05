//! In-process SSH server with a fake line-based shell.
//!
//! Shell commands (end lines with `\r` or `\n`):
//! - `size` prints the current PTY size as `COLSxROWS`
//! - `whoami` prints the authenticated user
//! - `exit N` exits with status `N`
//! - anything else is echoed back as `echo: <line>`
//!
//! - `agent-keys` lists the keys of the client's forwarded SSH agent
//!
//! `exec` supports `echo <text>`, `exit N` and `agent-keys`.
//!
//! The `sftp` subsystem serves [`TestServer::sftp_root`].
//!
//! The server also accepts `direct-tcpip` channels (local and dynamic
//! forwarding) and `tcpip-forward` requests (remote forwarding).
//!
//! On an `x11-req` it opens an X11 channel back to the client, sends an X11
//! setup request with the cookie it was given and records the first reply
//! (see [`TestServer::x11_replies`]).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::keys::PublicKey;
use russh::keys::agent::client::AgentClient;
use russh::server::{self, Auth, ChannelOpenHandle, Msg, Session};
use russh::{Channel, ChannelId, ChannelOpenFailure, Preferred, Pty};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

use crate::{TEST_CODE, TEST_PASSWORD, TEST_USER, private_key, public_key};

/// A running in-process SSH server. Stops when dropped.
#[derive(Debug)]
pub struct TestServer {
    pub addr: SocketAddr,
    pub host_key: PublicKey,
    task: JoinHandle<()>,
    sessions: Arc<Mutex<Vec<server::Handle>>>,
    sftp_root: tempfile::TempDir,
    x11_replies: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl TestServer {
    /// Start a server offering russh's default algorithms.
    pub async fn start() -> Self {
        Self::start_with(Preferred::default()).await
    }

    /// Start a server offering only the given algorithms, e.g. to emulate a
    /// legacy device.
    pub async fn start_with(preferred: Preferred) -> Self {
        Self::start_inner(preferred, false).await
    }

    /// Start a server that refuses password logins and asks through
    /// keyboard-interactive for the password ("Password: ", hidden) and
    /// then [`TEST_CODE`] ("Verification code: ", shown), like servers
    /// with two-factor login. Public keys still work.
    pub async fn start_keyboard_interactive() -> Self {
        Self::start_inner(Preferred::default(), true).await
    }

    async fn start_inner(preferred: Preferred, interactive: bool) -> Self {
        let host_key = private_key("host_ed25519");
        let methods = if interactive {
            russh::MethodSet::from(
                &[
                    russh::MethodKind::PublicKey,
                    russh::MethodKind::KeyboardInteractive,
                ][..],
            )
        } else {
            russh::MethodSet::from(&[russh::MethodKind::PublicKey, russh::MethodKind::Password][..])
        };
        let host_public = host_key.public_key().clone();
        let config = Arc::new(server::Config {
            keys: vec![host_key],
            preferred,
            auth_rejection_time: Duration::ZERO,
            auth_rejection_time_initial: Some(Duration::ZERO),
            inactivity_timeout: Some(Duration::from_secs(60)),
            methods,
            ..Default::default()
        });
        let authorized: Arc<[PublicKey]> = ["id_ed25519", "id_ed25519_encrypted", "id_rsa"]
            .map(public_key)
            .into();

        let sftp_root = tempfile::tempdir().expect("sftp root");
        let root = sftp_root.path().to_owned();
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let sessions: Arc<Mutex<Vec<server::Handle>>> = Arc::default();
        let tracked = sessions.clone();
        let x11_replies: Arc<Mutex<Vec<Vec<u8>>>> = Arc::default();
        let x11_log = x11_replies.clone();
        let task = tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let config = config.clone();
                let handler = ShellHandler {
                    authorized: authorized.clone(),
                    user: String::new(),
                    shells: HashMap::new(),
                    agent_forwarding: false,
                    forwards: Vec::new(),
                    channels: HashMap::new(),
                    sftp_root: root.clone(),
                    interactive,
                    interactive_step: 0,
                    x11_replies: x11_replies.clone(),
                };
                let tracked = tracked.clone();
                tokio::spawn(async move {
                    if let Ok(session) = server::run_stream(config, socket, handler).await {
                        tracked
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push(session.handle());
                        let _ = session.await;
                    }
                });
            }
        });
        Self {
            addr,
            host_key: host_public,
            task,
            sessions,
            sftp_root,
            x11_replies: x11_log,
        }
    }

    /// What the client's X display answered on forwarded X11 connections.
    pub fn x11_replies(&self) -> Vec<Vec<u8>> {
        self.x11_replies
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Directory served by the `sftp` subsystem.
    pub fn sftp_root(&self) -> &Path {
        self.sftp_root.path()
    }

    /// Disconnect every open session (as if the server restarted) while
    /// still accepting new connections.
    pub async fn drop_connections(&self) {
        let sessions: Vec<server::Handle> = self
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
            .collect();
        for session in sessions {
            let _ = session
                .disconnect(russh::Disconnect::ByApplication, "test".into(), "en".into())
                .await;
        }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Default)]
struct FakeShell {
    cols: u32,
    rows: u32,
    line: Vec<u8>,
}

struct ShellHandler {
    authorized: Arc<[PublicKey]>,
    user: String,
    shells: HashMap<ChannelId, FakeShell>,
    agent_forwarding: bool,
    /// Listeners for remote forwards; stopped with the session.
    forwards: Vec<JoinHandle<()>>,
    /// Session channels, kept until a subsystem takes one over.
    channels: HashMap<ChannelId, Channel<Msg>>,
    sftp_root: PathBuf,
    /// Password logins go through keyboard-interactive, with a code.
    interactive: bool,
    /// Keyboard-interactive questions answered correctly so far.
    interactive_step: u8,
    x11_replies: Arc<Mutex<Vec<Vec<u8>>>>,
}

/// An X11 setup request (big-endian, protocol 11.0) with this cookie.
fn x11_setup(protocol: &str, cookie: &[u8]) -> Vec<u8> {
    let pad = |len: usize| len.div_ceil(4) * 4 - len;
    let mut out = vec![b'B', 0, 0, 11, 0, 0];
    out.extend((protocol.len() as u16).to_be_bytes());
    out.extend((cookie.len() as u16).to_be_bytes());
    out.extend([0, 0]);
    out.extend_from_slice(protocol.as_bytes());
    out.extend(std::iter::repeat_n(0, pad(protocol.len())));
    out.extend_from_slice(cookie);
    out.extend(std::iter::repeat_n(0, pad(cookie.len())));
    out
}

impl Drop for ShellHandler {
    fn drop(&mut self) {
        for forward in &self.forwards {
            forward.abort();
        }
    }
}

/// Ask the client's forwarded agent for its keys and describe them.
async fn list_agent_keys(handle: &server::Handle) -> String {
    let Ok(channel) = handle.channel_open_agent().await else {
        return "agent: channel refused".into();
    };
    let mut agent = AgentClient::connect(channel.into_stream());
    match agent.request_identities().await {
        Ok(keys) => {
            let comments: Vec<String> = keys.iter().map(|k| k.comment().to_owned()).collect();
            format!("agent: {} keys: {}", keys.len(), comments.join(","))
        }
        Err(e) => format!("agent: error {e}"),
    }
}

impl ShellHandler {
    fn run_line(
        &mut self,
        channel: ChannelId,
        line: &str,
        session: &mut Session,
    ) -> Result<(), russh::Error> {
        let shell = self.shells.entry(channel).or_default();
        let line = line.trim();
        if let Some(code) = line.strip_prefix("exit") {
            let code = code.trim().parse().unwrap_or(0);
            session.exit_status_request(channel, code)?;
            session.eof(channel)?;
            return session.close(channel);
        }
        if line == "agent-keys" {
            if !self.agent_forwarding {
                return session.data(channel, b"agent: not forwarded\r\n$ ".to_vec());
            }
            let handle = session.handle();
            tokio::spawn(async move {
                let text = list_agent_keys(&handle).await;
                let _ = handle
                    .data(channel, format!("{text}\r\n$ ").into_bytes())
                    .await;
            });
            return Ok(());
        }
        let reply = match line {
            "" => String::new(),
            "size" => format!("{}x{}\r\n", shell.cols, shell.rows),
            "whoami" => format!("{}\r\n", self.user),
            other => format!("echo: {other}\r\n"),
        };
        session.data(channel, format!("{reply}$ ").into_bytes())
    }
}

impl server::Handler for ShellHandler {
    type Error = russh::Error;

    async fn auth_keyboard_interactive<'a>(
        &'a mut self,
        user: &str,
        _submethods: &str,
        response: Option<server::Response<'a>>,
    ) -> Result<Auth, Self::Error> {
        if !self.interactive || user != TEST_USER {
            return Ok(Auth::reject());
        }
        let answer = response.and_then(|mut r| r.next());
        let expected = match self.interactive_step {
            0 if answer.is_none() => {
                self.interactive_step = 1;
                return Ok(Auth::Partial {
                    name: "".into(),
                    instructions: "".into(),
                    prompts: vec![("Password: ".into(), false)].into(),
                });
            }
            1 => TEST_PASSWORD,
            _ => TEST_CODE,
        };
        if answer.as_deref() != Some(expected.as_bytes()) {
            self.interactive_step = 0;
            return Ok(Auth::reject());
        }
        if self.interactive_step == 1 {
            self.interactive_step = 2;
            return Ok(Auth::Partial {
                name: "Two-factor login".into(),
                instructions: "Enter the code from your app.".into(),
                prompts: vec![("Verification code: ".into(), true)].into(),
            });
        }
        self.user = user.to_owned();
        Ok(Auth::Accept)
    }

    async fn x11_request(
        &mut self,
        channel: ChannelId,
        _single_connection: bool,
        protocol: &str,
        cookie: &str,
        _screen: u32,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let Ok(cookie) = hex::decode(cookie) else {
            return Ok(());
        };
        let setup = x11_setup(protocol, &cookie);
        let handle = session.handle();
        let replies = self.x11_replies.clone();
        session.channel_success(channel)?;
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let Ok(x11) = handle.channel_open_x11("127.0.0.1", 6000).await else {
                return;
            };
            let mut stream = x11.into_stream();
            if stream.write_all(&setup).await.is_err() {
                return;
            }
            let mut reply = vec![0u8; 256];
            if let Ok(n) = stream.read(&mut reply).await {
                reply.truncate(n);
                replies
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(reply);
            }
        });
        Ok(())
    }

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        if self.interactive {
            return Ok(Auth::reject());
        }
        if user == TEST_USER && password == TEST_PASSWORD {
            self.user = user.to_owned();
            return Ok(Auth::Accept);
        }
        Ok(Auth::reject())
    }

    async fn auth_publickey(
        &mut self,
        user: &str,
        key: server::VerifiedPublicKey<'_>,
    ) -> Result<Auth, Self::Error> {
        if user == TEST_USER
            && self
                .authorized
                .iter()
                .any(|k| k.key_data() == key.public_key.key_data())
        {
            self.user = user.to_owned();
            return Ok(Auth::Accept);
        }
        Ok(Auth::reject())
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.shells.insert(channel.id(), FakeShell::default());
        self.channels.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn pty_request(
        &mut self,
        channel: ChannelId,
        _term: &str,
        cols: u32,
        rows: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let shell = self.shells.entry(channel).or_default();
        shell.cols = cols;
        shell.rows = rows;
        session.channel_success(channel)
    }

    async fn window_change_request(
        &mut self,
        channel: ChannelId,
        cols: u32,
        rows: u32,
        _pix_width: u32,
        _pix_height: u32,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        let shell = self.shells.entry(channel).or_default();
        shell.cols = cols;
        shell.rows = rows;
        Ok(())
    }

    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)?;
        session.data(channel, b"$ ".to_vec())
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        command: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)?;
        let command = String::from_utf8_lossy(command);
        if command == "agent-keys" {
            let handle = session.handle();
            let forwarded = self.agent_forwarding;
            tokio::spawn(async move {
                let text = if forwarded {
                    list_agent_keys(&handle).await
                } else {
                    "agent: not forwarded".into()
                };
                let _ = handle.data(channel, format!("{text}\n").into_bytes()).await;
                let _ = handle.exit_status_request(channel, 0).await;
                let _ = handle.eof(channel).await;
                let _ = handle.close(channel).await;
            });
            return Ok(());
        }
        let status = if let Some(text) = command.strip_prefix("echo ") {
            session.data(channel, format!("{text}\n").into_bytes())?;
            0
        } else if let Some(code) = command.strip_prefix("exit ") {
            code.trim().parse().unwrap_or(0)
        } else {
            session.extended_data(
                channel,
                1,
                format!("unknown command: {command}\n").into_bytes(),
            )?;
            127
        };
        session.exit_status_request(channel, status)?;
        session.eof(channel)?;
        session.close(channel)
    }

    async fn subsystem_request(
        &mut self,
        channel: ChannelId,
        name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        match (name, self.channels.remove(&channel)) {
            ("sftp", Some(stream)) => {
                // The SFTP server owns this channel's data from now on.
                self.shells.remove(&channel);
                session.channel_success(channel)?;
                let sftp = crate::sftp::DirSftp::new(self.sftp_root.clone());
                russh_sftp::server::run(stream.into_stream(), sftp).await;
            }
            _ => session.channel_failure(channel)?,
        }
        Ok(())
    }

    async fn agent_request(
        &mut self,
        _channel: ChannelId,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        self.agent_forwarding = true;
        Ok(true)
    }

    async fn channel_open_direct_tcpip(
        &mut self,
        channel: Channel<Msg>,
        host: &str,
        port: u32,
        _originator_address: &str,
        _originator_port: u32,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        let target = u16::try_from(port).ok();
        let tcp = match target {
            Some(port) => TcpStream::connect((host, port)).await.ok(),
            None => None,
        };
        let Some(mut tcp) = tcp else {
            reply.reject(ChannelOpenFailure::ConnectFailed).await;
            return Ok(());
        };
        reply.accept().await;
        tokio::spawn(async move {
            let mut stream = channel.into_stream();
            let _ = tokio::io::copy_bidirectional(&mut stream, &mut tcp).await;
        });
        Ok(())
    }

    async fn tcpip_forward(
        &mut self,
        address: &str,
        port: &mut u32,
        session: &mut Session,
    ) -> Result<bool, Self::Error> {
        let bind_ip = match address {
            "" | "localhost" | "0.0.0.0" => "127.0.0.1",
            other => other,
        };
        let Ok(requested) = u16::try_from(*port) else {
            return Ok(false);
        };
        let Ok(listener) = TcpListener::bind((bind_ip, requested)).await else {
            return Ok(false);
        };
        let Ok(local) = listener.local_addr() else {
            return Ok(false);
        };
        *port = local.port().into();
        let (handle, address, port) = (session.handle(), address.to_owned(), *port);
        self.forwards.push(tokio::spawn(async move {
            while let Ok((mut tcp, peer)) = listener.accept().await {
                let Ok(channel) = handle
                    .channel_open_forwarded_tcpip(
                        address.clone(),
                        port,
                        peer.ip().to_string(),
                        peer.port().into(),
                    )
                    .await
                else {
                    continue;
                };
                tokio::spawn(async move {
                    let mut stream = channel.into_stream();
                    let _ = tokio::io::copy_bidirectional(&mut stream, &mut tcp).await;
                });
            }
        }));
        Ok(true)
    }

    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        // Only session channels run the fake shell; forwarding channels
        // carry their own data.
        if !self.shells.contains_key(&channel) {
            return Ok(());
        }
        for &byte in data {
            if byte == b'\r' || byte == b'\n' {
                let shell = self.shells.entry(channel).or_default();
                let line = String::from_utf8_lossy(&std::mem::take(&mut shell.line)).into_owned();
                session.data(channel, b"\r\n".to_vec())?;
                self.run_line(channel, &line, session)?;
            } else {
                self.shells.entry(channel).or_default().line.push(byte);
                session.data(channel, vec![byte])?;
            }
        }
        Ok(())
    }
}
