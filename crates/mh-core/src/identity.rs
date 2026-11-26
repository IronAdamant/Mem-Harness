//! One agent name per agent, used by every tool.
//!
//! Libellus and CoordinationHub each name Claude Code sub-agents their own
//! way. The harness picks one scheme (Libellus's, since its names are the
//! strictest: lowercase slugs) and passes that name to every tool, so
//! notes, board tasks and locks of one agent share a key.

/// Lowercase letters, digits and `-`, 1 to 64 bytes, starting with a
/// letter or digit, and not a Windows device name.
pub fn valid_agent(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
        && !reserved_name(s)
}

fn reserved_name(seg: &str) -> bool {
    let stem = seg.split('.').next().unwrap_or(seg).to_ascii_lowercase();
    match stem.as_str() {
        "con" | "prn" | "aux" | "nul" => true,
        _ => {
            let b = stem.as_bytes();
            b.len() == 4
                && (stem.starts_with("com") || stem.starts_with("lpt"))
                && (b'1'..=b'9').contains(&b[3])
        }
    }
}

fn slug(text: &str, fallback: &str) -> String {
    let mut s: String = text
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    s.truncate(48);
    let s = s.trim_matches('-').to_string();
    if s.is_empty() {
        fallback.to_string()
    } else if reserved_name(&s) {
        format!("{s}-{fallback}")
    } else {
        s
    }
}

/// The name for a Claude Code sub-agent: `<agent_type>-<last 8 letters and
/// digits of agent_id>`. Matches Libellus 1.4 `subagent_name` exactly, so a
/// store written by Libellus's own hook keeps working.
pub fn subagent_name(agent_type: &str, agent_id: &str) -> String {
    let mut kind: String = slug(agent_type, "sub").chars().take(40).collect();
    while kind.ends_with('-') {
        kind.pop();
    }
    let id = agent_id.strip_prefix("agent-").unwrap_or(agent_id);
    let tail: Vec<char> = id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    let short: String = tail[tail.len().saturating_sub(8)..].iter().collect();
    if short.is_empty() {
        kind
    } else {
        format!("{kind}-{short}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_libellus_naming() {
        assert_eq!(
            subagent_name("Explore", "agent-a1b2c3d4e5f6"),
            "explore-c3d4e5f6"
        );
        assert_eq!(
            subagent_name("general-purpose", "xyz"),
            "general-purpose-xyz"
        );
        assert_eq!(subagent_name("", ""), "sub");
        assert_eq!(subagent_name("con", "1"), "con-sub-1");
    }

    #[test]
    fn validates_names() {
        assert!(valid_agent("main"));
        assert!(valid_agent("explore-abc123"));
        assert!(!valid_agent("Main"));
        assert!(!valid_agent("-x"));
        assert!(!valid_agent("nul"));
        assert!(!valid_agent(""));
    }
}
