use serde::{Deserialize, Serialize};

#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum HarnessId {
    #[default]
    Claude,
    Codex,
    Opencode,
}

impl HarnessId {
    pub const ALL: [Self; 3] = [Self::Claude, Self::Codex, Self::Opencode];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Opencode => "opencode",
        }
    }
}
