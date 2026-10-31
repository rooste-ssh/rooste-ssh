//! A saved connection.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::Protocol;

/// A saved session. Passwords are never stored here; see
/// [`SecretStore`](crate::SecretStore).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
    /// Stable identifier, also the key for the stored password.
    pub id: String,
    pub name: String,
    /// Folder path in the session manager, `/`-separated; empty for the top level.
    pub folder: String,
    pub tags: Vec<String>,
    pub protocol: Protocol,
    pub host: String,
    /// `None` uses the protocol's default port.
    pub port: Option<u16>,
    pub user: String,
    pub key_file: Option<PathBuf>,
    pub use_agent: bool,
    /// A password for this profile is kept in the OS credential store.
    pub save_password: bool,
    /// Offer outdated algorithms for old devices.
    pub allow_legacy: bool,
    /// Jump host(s), as in OpenSSH `ProxyJump`.
    pub proxy_jump: Option<String>,
    /// Forward the local SSH agent to the server.
    pub forward_agent: bool,
    /// Show the server's graphical (X11) programs on the local display.
    pub forward_x11: bool,
    /// A script (file name in the scripts folder) to run once connected;
    /// empty for none.
    pub on_connect_script: String,
    /// Port forwards started with the session.
    pub tunnels: Vec<Tunnel>,
    /// Line settings for serial profiles (the port name is in `host`).
    pub serial: SerialSettings,
    pub notes: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Parity {
    #[default]
    None,
    Odd,
    Even,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FlowControl {
    #[default]
    None,
    /// Software flow control.
    XonXoff,
    /// Hardware flow control.
    RtsCts,
}

/// Serial line settings, 9600 8N1 by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SerialSettings {
    pub baud: u32,
    /// 5 to 8.
    pub data_bits: u8,
    pub parity: Parity,
    /// 1 or 2.
    pub stop_bits: u8,
    pub flow_control: FlowControl,
}

impl Default for SerialSettings {
    fn default() -> Self {
        Self {
            baud: 9600,
            data_bits: 8,
            parity: Parity::None,
            stop_bits: 1,
            flow_control: FlowControl::None,
        }
    }
}

impl SerialSettings {
    /// Common baud rates, for pickers.
    pub const BAUD_RATES: [u32; 12] = [
        300, 1200, 2400, 4800, 9600, 19200, 38400, 57600, 115200, 230400, 460800, 921600,
    ];

    /// Short form such as `115200 8N1`.
    pub fn describe(&self) -> String {
        let parity = match self.parity {
            Parity::None => 'N',
            Parity::Odd => 'O',
            Parity::Even => 'E',
        };
        format!("{} {}{parity}{}", self.baud, self.data_bits, self.stop_bits)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TunnelKind {
    /// Local port to a host reachable from the server (`ssh -L`).
    #[default]
    Local,
    /// Server port to a host reachable from here (`ssh -R`).
    Remote,
    /// Local SOCKS proxy (`ssh -D`).
    Dynamic,
}

/// A saved port forward.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Tunnel {
    pub kind: TunnelKind,
    /// Where to listen: `port` or `address:port`. Local and dynamic
    /// forwards listen here; remote forwards on the server.
    pub listen: String,
    /// `host:port` to connect to; unused for dynamic forwards.
    pub target: String,
}

impl Tunnel {
    /// The listen address with a default bind address added.
    pub fn listen_addr(&self) -> Result<(String, u16), String> {
        split_host_port(&self.listen, "127.0.0.1")
    }

    pub fn target_addr(&self) -> Result<(String, u16), String> {
        split_host_port(&self.target, "").and_then(|(h, p)| {
            if h.is_empty() {
                Err(format!("`{}` needs a host", self.target))
            } else {
                Ok((h, p))
            }
        })
    }

    /// Check the fields, so mistakes surface in the editor.
    pub fn validate(&self) -> Result<(), String> {
        self.listen_addr()?;
        if self.kind != TunnelKind::Dynamic {
            self.target_addr()?;
        }
        Ok(())
    }

    /// Short description like `L 8080 → db:5432`.
    pub fn describe(&self) -> String {
        match self.kind {
            TunnelKind::Local => format!("L {} -> {}", self.listen, self.target),
            TunnelKind::Remote => format!("R {} -> {}", self.listen, self.target),
            TunnelKind::Dynamic => format!("D {} (SOCKS)", self.listen),
        }
    }
}

/// `port`, `host:port` or `[v6]:port`.
fn split_host_port(text: &str, default_host: &str) -> Result<(String, u16), String> {
    let text = text.trim();
    let (host, port) = if let Some(rest) = text.strip_prefix('[') {
        let (host, port) = rest
            .split_once("]:")
            .ok_or_else(|| format!("`{text}` is not host:port"))?;
        (host.to_owned(), port)
    } else {
        match text.rsplit_once(':') {
            Some((host, port)) => (host.to_owned(), port),
            None => (default_host.to_owned(), text),
        }
    };
    let port = port
        .parse::<u16>()
        .map_err(|_| format!("`{text}` has an invalid port"))?;
    Ok((
        if host.is_empty() {
            default_host.to_owned()
        } else {
            host
        },
        port,
    ))
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            id: new_id(),
            name: String::new(),
            folder: String::new(),
            tags: Vec::new(),
            protocol: Protocol::Ssh,
            host: String::new(),
            port: None,
            user: String::new(),
            key_file: None,
            use_agent: true,
            save_password: false,
            allow_legacy: false,
            proxy_jump: None,
            forward_agent: false,
            forward_x11: false,
            on_connect_script: String::new(),
            tunnels: Vec::new(),
            serial: SerialSettings::default(),
            notes: String::new(),
        }
    }
}

/// A fresh random profile id.
pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

impl Profile {
    /// Port to connect to.
    pub fn effective_port(&self) -> u16 {
        self.port
            .or_else(|| self.protocol.default_port())
            .unwrap_or(22)
    }

    /// Name shown in lists: the profile name, or `user@host`.
    pub fn display_name(&self) -> String {
        if !self.name.trim().is_empty() {
            self.name.clone()
        } else if self.user.is_empty() {
            self.host.clone()
        } else {
            format!("{}@{}", self.user, self.host)
        }
    }

    /// Case-insensitive match against name, host, user, folder and tags.
    pub fn matches(&self, query: &str) -> bool {
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            return true;
        }
        query.split_whitespace().all(|word| {
            [&self.name, &self.host, &self.user, &self.folder]
                .into_iter()
                .chain(&self.tags)
                .any(|field| field.to_lowercase().contains(word))
        })
    }

    /// A copy with a new id and " (copy)" appended to the name.
    pub fn duplicate(&self) -> Self {
        Self {
            id: new_id(),
            name: format!("{} (copy)", self.display_name()),
            save_password: false,
            ..self.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Profile {
        Profile {
            name: "Web server".into(),
            folder: "Work/Production".into(),
            tags: vec!["nginx".into()],
            host: "web1.example.com".into(),
            user: "deploy".into(),
            ..Profile::default()
        }
    }

    #[test]
    fn serial_settings() {
        let s = SerialSettings::default();
        assert_eq!(s.describe(), "9600 8N1");
        let s = SerialSettings {
            baud: 115200,
            parity: Parity::Even,
            stop_bits: 2,
            data_bits: 7,
            ..s
        };
        assert_eq!(s.describe(), "115200 7E2");
    }

    #[test]
    fn tunnel_addresses() {
        let local = Tunnel {
            kind: TunnelKind::Local,
            listen: "8080".into(),
            target: "db.internal:5432".into(),
        };
        assert_eq!(local.listen_addr().unwrap(), ("127.0.0.1".into(), 8080));
        assert_eq!(local.target_addr().unwrap(), ("db.internal".into(), 5432));
        assert_eq!(local.describe(), "L 8080 -> db.internal:5432");
        assert!(local.validate().is_ok());

        let any = Tunnel {
            listen: "0.0.0.0:1080".into(),
            kind: TunnelKind::Dynamic,
            ..Tunnel::default()
        };
        assert_eq!(any.listen_addr().unwrap(), ("0.0.0.0".into(), 1080));
        assert!(any.validate().is_ok(), "dynamic needs no target");

        let v6 = Tunnel {
            listen: "[::1]:22".into(),
            target: "x:1".into(),
            ..Tunnel::default()
        };
        assert_eq!(v6.listen_addr().unwrap(), ("::1".into(), 22));

        for bad in ["", "abc", "host:99999"] {
            let t = Tunnel {
                listen: bad.into(),
                target: "x:1".into(),
                ..Tunnel::default()
            };
            assert!(t.validate().is_err(), "{bad}");
        }
        let no_target = Tunnel {
            listen: "1".into(),
            target: "5432".into(),
            ..Tunnel::default()
        };
        assert!(no_target.validate().is_err());
    }

    #[test]
    fn ids_are_unique() {
        assert_ne!(Profile::default().id, Profile::default().id);
    }

    #[test]
    fn effective_port_falls_back_to_protocol_default() {
        let mut p = sample();
        assert_eq!(p.effective_port(), 22);
        p.protocol = Protocol::Telnet;
        assert_eq!(p.effective_port(), 23);
        p.port = Some(2222);
        assert_eq!(p.effective_port(), 2222);
    }

    #[test]
    fn display_name_fallbacks() {
        let mut p = sample();
        assert_eq!(p.display_name(), "Web server");
        p.name.clear();
        assert_eq!(p.display_name(), "deploy@web1.example.com");
        p.user.clear();
        assert_eq!(p.display_name(), "web1.example.com");
    }

    #[test]
    fn search_matches_every_word_in_any_field() {
        let p = sample();
        assert!(p.matches(""));
        assert!(p.matches("WEB"));
        assert!(p.matches("production nginx"));
        assert!(p.matches("deploy web1"));
        assert!(!p.matches("staging"));
        assert!(!p.matches("web staging"));
    }

    #[test]
    fn duplicate_gets_new_id_and_no_saved_password() {
        let mut p = sample();
        p.save_password = true;
        let copy = p.duplicate();
        assert_ne!(copy.id, p.id);
        assert_eq!(copy.name, "Web server (copy)");
        assert!(!copy.save_password);
        assert_eq!(copy.host, p.host);
    }
}
