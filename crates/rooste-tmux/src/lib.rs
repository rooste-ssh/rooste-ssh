//! Sans-I/O state machine and parser for the tmux control mode (`tmux -CC`) protocol.

pub mod ids;
pub mod layout;
pub mod parser;
pub mod state;
pub mod unescape;

pub use ids::{ParseIdError, TmuxPaneId, TmuxSessionId, TmuxWindowId};
pub use layout::{LayoutError, LayoutGeometry, LayoutNode, parse_layout};
pub use parser::{ControlParser, TmuxEvent, TmuxNotification, TmuxResponse};
pub use state::{
    MachineEvent, TmuxClientAction, TmuxMachine, TmuxPaneSnapshot, TmuxWindowSnapshot,
};
pub use unescape::{
    DCS_TMUX_ENTER, DcsDetector, decode_tmux_output, encode_paste_hex, encode_send_keys_hex,
    escape_tmux_arg,
};
