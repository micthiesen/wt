use crate::{WorktreeRef, worktree_ledger_key};
use serde::{Deserialize, Deserializer, Serialize, de::Error as _};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteEndpoint {
    pub host: String,
    pub label: String,
    pub wt_path: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum WorktreeLocation {
    Local,
    Remote { endpoint: RemoteEndpoint },
}

/// One fleet member independent of where its checkout is materialized.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeTarget {
    #[serde(rename = "ref")]
    reference: WorktreeRef,
    slug: String,
    pub branch: String,
    pub path: String,
    pub stage: String,
    location: WorktreeLocation,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawWorktreeTarget {
    #[serde(rename = "ref")]
    reference: WorktreeRef,
    slug: String,
    branch: String,
    path: String,
    stage: String,
    location: WorktreeLocation,
}

impl<'de> Deserialize<'de> for WorktreeTarget {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = RawWorktreeTarget::deserialize(deserializer)?;
        let consistent = match (&raw.reference, &raw.location) {
            (WorktreeRef::Local { slug }, WorktreeLocation::Local) => slug == &raw.slug,
            (WorktreeRef::Remote { host, slug }, WorktreeLocation::Remote { endpoint }) => {
                host == &endpoint.host && slug == &raw.slug
            }
            _ => false,
        };
        if !consistent {
            return Err(D::Error::custom(
                "worktree target ref, slug, and location endpoint are inconsistent",
            ));
        }
        Ok(Self {
            reference: raw.reference,
            slug: raw.slug,
            branch: raw.branch,
            path: raw.path,
            stage: raw.stage,
            location: raw.location,
        })
    }
}

impl WorktreeTarget {
    pub fn reference(&self) -> &WorktreeRef {
        &self.reference
    }
    pub fn slug(&self) -> &str {
        &self.slug
    }
    pub fn location(&self) -> &WorktreeLocation {
        &self.location
    }
}

pub fn local_worktree_target(
    slug: impl Into<String>,
    branch: impl Into<String>,
    path: impl Into<String>,
    stage: impl Into<String>,
) -> WorktreeTarget {
    let slug = slug.into();
    WorktreeTarget {
        reference: WorktreeRef::Local { slug: slug.clone() },
        slug,
        branch: branch.into(),
        path: path.into(),
        stage: stage.into(),
        location: WorktreeLocation::Local,
    }
}

pub fn remote_worktree_target(
    slug: impl Into<String>,
    branch: impl Into<String>,
    path: impl Into<String>,
    stage: impl Into<String>,
    endpoint: RemoteEndpoint,
) -> WorktreeTarget {
    let slug = slug.into();
    WorktreeTarget {
        reference: WorktreeRef::Remote {
            host: endpoint.host.clone(),
            slug: slug.clone(),
        },
        slug,
        branch: branch.into(),
        path: path.into(),
        stage: stage.into(),
        location: WorktreeLocation::Remote { endpoint },
    }
}

pub fn worktree_target_key(target: &WorktreeTarget) -> String {
    worktree_ledger_key(&target.reference)
}

/// Stable host-qualified action/tmux slot, retaining the historical bare key
/// for local worktrees.
pub fn worktree_action_key(target: &WorktreeTarget) -> String {
    match &target.location {
        WorktreeLocation::Local => target.slug.clone(),
        WorktreeLocation::Remote { endpoint } => {
            remote_worktree_action_key(&endpoint.host, &target.slug)
        }
    }
}

pub fn remote_worktree_action_key(hostname: &str, slug: &str) -> String {
    let digest = Sha256::digest(hostname.as_bytes());
    let mut hex = String::with_capacity(10);
    for byte in digest.iter().take(5) {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    format!("remote-{hex}-{slug}")
}

pub fn is_remote_worktree_target(target: &WorktreeTarget) -> bool {
    matches!(target.location, WorktreeLocation::Remote { .. })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(host: &str) -> RemoteEndpoint {
        RemoteEndpoint {
            host: host.into(),
            label: host.into(),
            wt_path: "~/bin/wt".into(),
        }
    }

    #[test]
    fn local_and_remote_targets_share_metadata_and_keep_distinct_identity() {
        let local = local_worktree_target("same", "team/same", "/work/same", "team-same");
        let a = remote_worktree_target(
            "same",
            "team/same",
            "/work/same",
            "team-same",
            endpoint("builder-a"),
        );
        let b = remote_worktree_target(
            "same",
            "team/same",
            "/work/same",
            "team-same",
            endpoint("builder-b"),
        );
        assert_eq!(
            (
                local.branch.as_str(),
                local.path.as_str(),
                local.stage.as_str()
            ),
            (a.branch.as_str(), a.path.as_str(), a.stage.as_str())
        );
        assert!(!is_remote_worktree_target(&local));
        assert!(is_remote_worktree_target(&a));
        assert_ne!(worktree_target_key(&a), worktree_target_key(&b));
        assert_ne!(worktree_action_key(&a), worktree_action_key(&b));
        assert_eq!(worktree_action_key(&local), "same");
    }

    #[test]
    fn action_key_digest_matches_sha256_hex_prefix() {
        assert_eq!(
            remote_worktree_action_key("builder-a", "same"),
            "remote-7f532b6a4e-same"
        );
    }

    #[test]
    fn target_serialization_round_trips_but_rejects_mismatched_identity() {
        let target = remote_worktree_target(
            "same",
            "team/same",
            "/work/same",
            "team-same",
            endpoint("builder-a"),
        );
        let encoded = serde_json::to_value(&target).unwrap();
        assert_eq!(
            serde_json::from_value::<WorktreeTarget>(encoded.clone()).unwrap(),
            target
        );

        for invalid in [
            serde_json::json!({"ref":{"kind":"remote","host":"builder-b","slug":"same"}, "slug":"same", "branch":"b", "path":"p", "stage":"s", "location":{"kind":"remote","endpoint":{"host":"builder-a","label":"a","wtPath":"wt"}}}),
            serde_json::json!({"ref":{"kind":"remote","host":"builder-a","slug":"other"}, "slug":"same", "branch":"b", "path":"p", "stage":"s", "location":{"kind":"remote","endpoint":{"host":"builder-a","label":"a","wtPath":"wt"}}}),
            serde_json::json!({"ref":{"kind":"remote","host":"builder-a","slug":"same"}, "slug":"same", "branch":"b", "path":"p", "stage":"s", "location":{"kind":"local"}}),
        ] {
            assert!(serde_json::from_value::<WorktreeTarget>(invalid).is_err());
        }
    }
}
