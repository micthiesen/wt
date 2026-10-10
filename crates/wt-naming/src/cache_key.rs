use sha2::{Digest, Sha256};

use crate::StackMember;

/// Cache namespaces mirror the legacy query cache's distinct result classes.
/// The caller persists these keys and values in its own store.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NamingCacheKey {
    Diff(String),
    Manual(String),
    Stack(String),
}

impl NamingCacheKey {
    pub fn as_str(&self) -> String {
        match self {
            Self::Diff(hash) => format!("aiSummary:{hash}"),
            Self::Manual(slug) => format!("manualSummary:{slug}"),
            Self::Stack(signature) => format!("stackTitle:{signature}"),
        }
    }
}

pub fn diff_cache_key(hash: impl Into<String>) -> NamingCacheKey {
    NamingCacheKey::Diff(hash.into())
}

pub fn stack_cache_key(members: &[StackMember]) -> NamingCacheKey {
    NamingCacheKey::Stack(stack_signature(members))
}

/// Stable stack membership signature. Branch order and member titles do not
/// affect the key; membership changes do. The v2 salt intentionally orphans
/// titles generated before the structured prompt was ready.
pub fn stack_signature(members: &[StackMember]) -> String {
    if members.is_empty() {
        return "__empty__".into();
    }
    let mut branches: Vec<_> = members
        .iter()
        .map(|member| member.branch.as_str())
        .collect();
    branches.sort_unstable();
    let mut hash = Sha256::new();
    hash.update(b"v2");
    for branch in branches {
        hash.update([0]);
        hash.update(branch.as_bytes());
    }
    short_hex(hash.finalize().as_slice())
}

pub(crate) fn short_hash(bytes: &[u8]) -> String {
    short_hex(&Sha256::digest(bytes))
}

fn short_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(16);
    for byte in bytes.iter().take(8) {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stack_key_sorts_branches_and_ignores_member_titles() {
        let a = vec![
            StackMember {
                branch: "z/second".into(),
                title: "one title".into(),
            },
            StackMember {
                branch: "a/first".into(),
                title: "other title".into(),
            },
        ];
        let b = vec![
            StackMember {
                branch: "a/first".into(),
                title: "changed title".into(),
            },
            StackMember {
                branch: "z/second".into(),
                title: "changed too".into(),
            },
        ];
        assert_eq!(stack_signature(&a), stack_signature(&b));
        assert_ne!(
            stack_signature(&a),
            stack_signature(&[StackMember {
                branch: "different".into(),
                title: String::new(),
            }])
        );
        assert_eq!(stack_signature(&[]), "__empty__");
        assert_eq!(diff_cache_key("abc").as_str(), "aiSummary:abc");
    }
}
