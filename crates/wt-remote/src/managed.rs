//! Typed contract between a controller and the config-free `_managed`
//! command of the same native build. The controller decides every file a
//! managed worker owns; the worker writes them, ensures its clone, and reports
//! what still needs a person (a sign-in) or an install.

use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Bumped when the request or report shape changes incompatibly. Both sides
/// are always the same build, so a mismatch means a corrupted exchange.
pub const MANAGED_PROTOCOL: u32 = 1;

/// Replaced in file contents by the worker with the absolute managed root,
/// because only the worker knows its own home directory.
pub const ROOT_PLACEHOLDER: &str = "@@WT_MANAGED_ROOT@@";

/// The local port Codex's browser sign-in listens on for its callback.
pub const CODEX_LOGIN_PORT: u16 = 1455;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedRequest {
    pub protocol: u32,
    /// Home-relative directory every managed file lives under.
    pub root: String,
    /// HTTPS clone URL of the controller's `origin`.
    pub repository: String,
    /// Root-relative files, rewritten only when their content changes.
    pub files: Vec<ManagedFile>,
    /// Root-relative files to delete, such as a credential the controller no
    /// longer has.
    #[serde(default)]
    pub remove: Vec<String>,
    /// Root-relative directories mirrored exactly; extra files are removed.
    pub trees: Vec<ManagedTree>,
    /// Untracked controller files copied into the managed main clone.
    pub repo_files: Vec<ManagedFile>,
    /// The controller's Codex `config.toml`, adapted by the worker.
    pub codex_config: Option<String>,
    /// Credential-free controller account-switch intent; workers log in independently.
    #[serde(default)]
    pub codex_auth_generation: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedFile {
    pub path: String,
    #[serde(serialize_with = "encode_bytes", deserialize_with = "decode_bytes")]
    pub content: Vec<u8>,
    pub mode: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedTree {
    pub path: String,
    pub files: Vec<ManagedFile>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SignIn {
    Codex,
    Github,
}

impl SignIn {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Github => "github",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Github => "GitHub",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "codex" => Some(Self::Codex),
            "github" => Some(Self::Github),
            _ => None,
        }
    }

    /// A browser sign-in whose callback must reach the worker's loopback.
    pub fn forward_port(self) -> Option<u16> {
        match self {
            Self::Codex => Some(CODEX_LOGIN_PORT),
            Self::Github => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CheckState {
    Ok,
    /// Usable, with a degraded optional capability.
    Warning,
    /// A person must sign in before the capability works.
    SignInNeeded,
    /// Broken until something outside wt changes, such as an install.
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedCheck {
    pub id: String,
    pub state: CheckState,
    pub detail: String,
    #[serde(default)]
    pub sign_in: Option<SignIn>,
}

impl ManagedCheck {
    pub fn ok(id: &str, detail: impl Into<String>) -> Self {
        Self::new(id, CheckState::Ok, detail, None)
    }

    pub fn new(
        id: &str,
        state: CheckState,
        detail: impl Into<String>,
        sign_in: Option<SignIn>,
    ) -> Self {
        Self {
            id: id.to_owned(),
            state,
            detail: detail.into(),
            sign_in,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedReport {
    pub protocol: u32,
    pub checks: Vec<ManagedCheck>,
    /// Latest controller generation this worker has received, not proof of login.
    #[serde(default)]
    pub codex_auth_generation: Option<u64>,
}

impl ManagedReport {
    /// Sign-ins still owed, in a stable order.
    pub fn sign_ins(&self) -> Vec<SignIn> {
        let mut needed = self
            .checks
            .iter()
            .filter(|check| check.state == CheckState::SignInNeeded)
            .filter_map(|check| check.sign_in)
            .collect::<Vec<_>>();
        needed.sort();
        needed.dedup();
        needed
    }

    /// The worker cannot run without its clone; every other check degrades
    /// only the capability it names.
    pub fn blocking_failure(&self) -> Option<&ManagedCheck> {
        self.checks
            .iter()
            .find(|check| check.id == "repository" && check.state != CheckState::Ok)
    }
}

fn encode_bytes<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&STANDARD.encode(bytes))
}

fn decode_bytes<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
    let text = String::deserialize(deserializer)?;
    STANDARD.decode(text).map_err(serde::de::Error::custom)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trips_binary_file_content() {
        let request = ManagedRequest {
            protocol: MANAGED_PROTOCOL,
            root: ".local/share/wt/managed/app".into(),
            repository: "https://github.com/acme/app.git".into(),
            files: vec![ManagedFile {
                path: "secrets/token".into(),
                content: vec![0, 159, 146, 150, b'\n'],
                mode: 0o600,
            }],
            remove: vec![],
            trees: vec![],
            repo_files: vec![],
            codex_config: None,
            codex_auth_generation: None,
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(!json.contains("\\u0000"));
        assert_eq!(
            serde_json::from_str::<ManagedRequest>(&json).unwrap(),
            request
        );
    }

    #[test]
    fn report_lists_each_owed_sign_in_once_and_keeps_warnings_as_notices() {
        let report = ManagedReport {
            protocol: MANAGED_PROTOCOL,
            codex_auth_generation: None,
            checks: vec![
                ManagedCheck::new(
                    "codex-login",
                    CheckState::SignInNeeded,
                    "Codex",
                    Some(SignIn::Codex),
                ),
                ManagedCheck::new(
                    "github",
                    CheckState::SignInNeeded,
                    "GitHub",
                    Some(SignIn::Github),
                ),
                ManagedCheck::new(
                    "codex-again",
                    CheckState::SignInNeeded,
                    "Codex",
                    Some(SignIn::Codex),
                ),
                ManagedCheck::new("gh", CheckState::Warning, "gh missing", None),
                ManagedCheck::ok("repository", "cloned"),
            ],
        };
        assert_eq!(report.sign_ins(), [SignIn::Codex, SignIn::Github]);
        assert!(report.blocking_failure().is_none());
    }
}
