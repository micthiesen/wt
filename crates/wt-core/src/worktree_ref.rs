use serde::{Deserialize, Serialize};

const REMOTE_LEDGER_PREFIX: &str = "@remote/";

/// Fleet identity is independent of where the checkout is materialized.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum WorktreeRef {
    Local { slug: String },
    Remote { host: String, slug: String },
}

pub fn worktree_ledger_key(reference: &WorktreeRef) -> String {
    match reference {
        WorktreeRef::Local { slug } => slug.clone(),
        WorktreeRef::Remote { host, slug } => {
            format!(
                "{REMOTE_LEDGER_PREFIX}{}/{}",
                encode_uri_component(host),
                encode_uri_component(slug)
            )
        }
    }
}

pub fn remote_worktree_ledger_key(host: &str, slug: &str) -> String {
    worktree_ledger_key(&WorktreeRef::Remote {
        host: host.to_owned(),
        slug: slug.to_owned(),
    })
}

pub fn remote_worktree_ledger_prefix(host: &str) -> String {
    format!("{REMOTE_LEDGER_PREFIX}{}/", encode_uri_component(host))
}

pub fn is_remote_worktree_ledger_key(key: &str) -> bool {
    key.starts_with(REMOTE_LEDGER_PREFIX)
}

/// Decode only canonical keys. Malformed remote-looking keys never fall back
/// to local slugs, and aliases cannot address another host's persisted state.
pub fn parse_worktree_ledger_key(key: &str) -> Option<WorktreeRef> {
    if let Some(rest) = key.strip_prefix(REMOTE_LEDGER_PREFIX) {
        let (host, slug) = rest.split_once('/')?;
        let reference = WorktreeRef::Remote {
            host: decode_uri_component(host)?,
            slug: decode_uri_component(slug)?,
        };
        if host.is_empty() || slug.is_empty() || worktree_ledger_key(&reference) != key {
            return None;
        }
        Some(reference)
    } else if !key.is_empty() && !key.starts_with('@') && !key.chars().any(char::is_control) {
        Some(WorktreeRef::Local { slug: key.into() })
    } else {
        None
    }
}

/// Human-facing identity for logs sourced from a location-aware ledger key.
pub fn worktree_ledger_label(key: &str) -> String {
    let Some(rest) = key.strip_prefix(REMOTE_LEDGER_PREFIX) else {
        return key.to_owned();
    };
    let mut parts = rest.splitn(2, '/');
    let host = parts.next().unwrap_or("remote");
    let slug = parts.next().unwrap_or(key);
    match (decode_uri_component(host), decode_uri_component(slug)) {
        (Some(host), Some(slug)) => format!("{slug} @ {host}"),
        _ => key.to_owned(),
    }
}

fn encode_uri_component(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')'
            )
        {
            encoded.push(byte as char);
        } else {
            use std::fmt::Write as _;
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

fn decode_uri_component(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let high = *bytes.get(i + 1)?;
            let low = *bytes.get(i + 2)?;
            decoded.push(hex(high)? << 4 | hex(low)?);
            i += 3;
        } else {
            decoded.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

fn hex(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_slugs_keep_the_legacy_key() {
        let reference = WorktreeRef::Local {
            slug: "eng-123-fix".into(),
        };
        assert_eq!(worktree_ledger_key(&reference), "eng-123-fix");
    }

    #[test]
    fn remote_keys_are_encoded_and_endpoint_qualified() {
        let a = remote_worktree_ledger_key("builder/a", "feature/x");
        let b = remote_worktree_ledger_key("builder", "a/feature/x");
        assert_ne!(a, b);
        assert_eq!(a, "@remote/builder%2Fa/feature%2Fx");
        assert_eq!(
            remote_worktree_ledger_prefix("builder/a"),
            "@remote/builder%2Fa/"
        );
        assert!(is_remote_worktree_ledger_key(&a));
        assert!(!is_remote_worktree_ledger_key("eng-123-fix"));
        assert_eq!(worktree_ledger_label(&a), "feature/x @ builder/a");
    }

    #[test]
    fn malformed_remote_key_label_falls_back_to_the_original() {
        let key = "@remote/bad%2/slug";
        assert_eq!(worktree_ledger_label(key), key);
    }
}
