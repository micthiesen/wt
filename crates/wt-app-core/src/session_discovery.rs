//! Identity of a discovered coding-agent conversation. The session source
//! publishes these; harness session pickers read them.

use serde::{Deserialize, Serialize};
use wt_core::HarnessId;
use wt_harness::HarnessSession;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionKey {
    pub slug: String,
    pub harness: HarnessId,
    pub session_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredSession {
    pub key: SessionKey,
    pub session: HarnessSession,
}

/// Vector DTO so the stable composite identity is representable in JSON/SSH
/// protocol messages. Consumers index by `key`, never by a display label.
pub type SessionDiscoveries = Vec<DiscoveredSession>;
