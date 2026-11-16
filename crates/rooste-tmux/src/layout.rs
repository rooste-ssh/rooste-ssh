//! Recursive parser for tmux window layout strings.
//!
//! Format example:
//! `b25f,158x45,0,0{79x45,0,0,1,78x45,80,0[78x22,80,0,2,78x22,80,23,3]}`

use crate::ids::TmuxPaneId;

/// Error returned when a tmux layout string is malformed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid tmux layout `{input}`: {reason}")]
pub struct LayoutError {
    pub input: String,
    pub reason: &'static str,
}

/// Cell geometry of a layout node within a tmux window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutGeometry {
    pub cols: u16,
    pub rows: u16,
    pub x: u16,
    pub y: u16,
}

/// A node in a tmux window's split layout tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayoutNode {
    /// A leaf pane (`<cols>x<rows>,<x>,<y>,<pane_id>`).
    Pane {
        geom: LayoutGeometry,
        pane_id: TmuxPaneId,
    },
    /// Left-to-right horizontal split (`<cols>x<rows>,<x>,<y>{child1,child2,...}`).
    HorizontalSplit {
        geom: LayoutGeometry,
        children: Vec<LayoutNode>,
    },
    /// Top-to-bottom vertical split (`<cols>x<rows>,<x>,<y>[child1,child2,...]`).
    VerticalSplit {
        geom: LayoutGeometry,
        children: Vec<LayoutNode>,
    },
}

impl LayoutNode {
    pub fn geometry(&self) -> LayoutGeometry {
        match self {
            Self::Pane { geom, .. }
            | Self::HorizontalSplit { geom, .. }
            | Self::VerticalSplit { geom, .. } => *geom,
        }
    }

    /// Collect all leaf panes and their geometries in depth-first order.
    pub fn panes(&self) -> Vec<(TmuxPaneId, LayoutGeometry)> {
        let mut out = Vec::new();
        self.collect_panes(&mut out);
        out
    }

    fn collect_panes(&self, out: &mut Vec<(TmuxPaneId, LayoutGeometry)>) {
        match self {
            Self::Pane { geom, pane_id } => out.push((*pane_id, *geom)),
            Self::HorizontalSplit { children, .. } | Self::VerticalSplit { children, .. } => {
                for child in children {
                    child.collect_panes(out);
                }
            }
        }
    }
}

/// Parse a full tmux layout string (with or without the leading 4-hex-digit checksum).
pub fn parse_layout(input: &str) -> Result<LayoutNode, LayoutError> {
    let trimmed = input.trim();
    let err = |reason: &'static str| LayoutError {
        input: trimmed.to_owned(),
        reason,
    };
    if trimmed.is_empty() {
        return Err(err("empty layout string"));
    }
    // Strip optional `<checksum>,` prefix if the first comma precedes any `x`.
    let body = match trimmed.split_once(',') {
        Some((prefix, rest))
            if prefix.len() == 4
                && prefix.chars().all(|c| c.is_ascii_hexdigit())
                && !prefix.contains('x') =>
        {
            rest
        }
        _ => trimmed,
    };
    let (node, rest) = parse_node(body).map_err(err)?;
    if !rest.is_empty() {
        return Err(err("unexpected trailing characters"));
    }
    Ok(node)
}

fn parse_node(s: &str) -> Result<(LayoutNode, &str), &'static str> {
    let (cols, s) = parse_u16_until(s, 'x')?;
    let (rows, s) = parse_u16_until(s, ',')?;
    let (x, s) = parse_u16_until(s, ',')?;
    let (y, s) = parse_u16_prefix(s)?;
    let geom = LayoutGeometry { cols, rows, x, y };

    match s.chars().next() {
        Some('{') => {
            let (children, rest) = parse_children(&s[1..], '}')?;
            Ok((LayoutNode::HorizontalSplit { geom, children }, rest))
        }
        Some('[') => {
            let (children, rest) = parse_children(&s[1..], ']')?;
            Ok((LayoutNode::VerticalSplit { geom, children }, rest))
        }
        Some(',') => {
            let (pane_id, rest) = parse_u32_prefix(&s[1..])?;
            Ok((
                LayoutNode::Pane {
                    geom,
                    pane_id: TmuxPaneId(pane_id),
                },
                rest,
            ))
        }
        _ => Err("expected '{', '[', or ',pane_id' after geometry"),
    }
}

fn parse_children(mut s: &str, closing: char) -> Result<(Vec<LayoutNode>, &str), &'static str> {
    let mut children = Vec::new();
    loop {
        let (child, rest) = parse_node(s)?;
        children.push(child);
        match rest.chars().next() {
            Some(',') => s = &rest[1..],
            Some(c) if c == closing => return Ok((children, &rest[1..])),
            _ => return Err("unclosed split container in layout"),
        }
    }
}

fn parse_u16_until(s: &str, delim: char) -> Result<(u16, &str), &'static str> {
    let (num, rest) = s
        .split_once(delim)
        .ok_or("missing delimiter in layout geometry")?;
    let val = num
        .parse::<u16>()
        .map_err(|_| "invalid number in layout geometry")?;
    Ok((val, rest))
}

fn parse_u16_prefix(s: &str) -> Result<(u16, &str), &'static str> {
    let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    if end == 0 {
        return Err("expected coordinate number");
    }
    let val = s[..end].parse::<u16>().map_err(|_| "coordinate overflow")?;
    Ok((val, &s[end..]))
}

fn parse_u32_prefix(s: &str) -> Result<(u32, &str), &'static str> {
    let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    if end == 0 {
        return Err("expected pane id");
    }
    let val = s[..end].parse::<u32>().map_err(|_| "pane id overflow")?;
    Ok((val, &s[end..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_pane_layout() {
        let node = parse_layout("b25f,80x24,0,0,0").unwrap();
        assert_eq!(
            node,
            LayoutNode::Pane {
                geom: LayoutGeometry {
                    cols: 80,
                    rows: 24,
                    x: 0,
                    y: 0,
                },
                pane_id: TmuxPaneId(0),
            }
        );
    }

    #[test]
    fn parses_nested_horizontal_and_vertical_splits() {
        let raw = "a1b2,159x48,0,0{79x48,0,0,1,79x48,80,0[79x23,80,0,2,79x24,80,24,5]}";
        let node = parse_layout(raw).unwrap();
        let panes = node.panes();
        assert_eq!(panes.len(), 3);
        assert_eq!(panes[0].0, TmuxPaneId(1));
        assert_eq!(panes[1].0, TmuxPaneId(2));
        assert_eq!(panes[2].0, TmuxPaneId(5));
    }
}
