//! Strongly-typed tmux identifiers (`$session`, `@window`, `%pane`).

use std::fmt;
use std::str::FromStr;

/// Error returned when parsing a tmux identifier fails.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid tmux identifier `{0}`")]
pub struct ParseIdError(pub String);

/// A tmux session ID (formatted as `$0`, `$1`, ...).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TmuxSessionId(pub u32);

impl fmt::Display for TmuxSessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "${}", self.0)
    }
}

impl FromStr for TmuxSessionId {
    type Err = ParseIdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let num = s
            .trim()
            .strip_prefix('$')
            .and_then(|rest| rest.parse::<u32>().ok())
            .ok_or_else(|| ParseIdError(s.to_owned()))?;
        Ok(Self(num))
    }
}

/// A tmux window ID (formatted as `@0`, `@1`, ...).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TmuxWindowId(pub u32);

impl fmt::Display for TmuxWindowId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "@{}", self.0)
    }
}

impl FromStr for TmuxWindowId {
    type Err = ParseIdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let num = s
            .trim()
            .strip_prefix('@')
            .and_then(|rest| rest.parse::<u32>().ok())
            .ok_or_else(|| ParseIdError(s.to_owned()))?;
        Ok(Self(num))
    }
}

/// A tmux pane ID (formatted as `%0`, `%1`, ...).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TmuxPaneId(pub u32);

impl fmt::Display for TmuxPaneId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "%{}", self.0)
    }
}

impl FromStr for TmuxPaneId {
    type Err = ParseIdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let num = s
            .trim()
            .strip_prefix('%')
            .and_then(|rest| rest.parse::<u32>().ok())
            .ok_or_else(|| ParseIdError(s.to_owned()))?;
        Ok(Self(num))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_ids() {
        assert_eq!("$0".parse::<TmuxSessionId>(), Ok(TmuxSessionId(0)));
        assert_eq!(TmuxSessionId(42).to_string(), "$42");
        assert_eq!("@7".parse::<TmuxWindowId>(), Ok(TmuxWindowId(7)));
        assert_eq!(TmuxWindowId(7).to_string(), "@7");
        assert_eq!("%12".parse::<TmuxPaneId>(), Ok(TmuxPaneId(12)));
        assert_eq!(TmuxPaneId(12).to_string(), "%12");

        assert!("0".parse::<TmuxSessionId>().is_err());
        assert!("$abc".parse::<TmuxSessionId>().is_err());
        assert!("%".parse::<TmuxPaneId>().is_err());
    }
}
