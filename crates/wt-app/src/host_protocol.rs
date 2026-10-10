//! One versioned stream for the shared host service. Terminal handoff remains
//! local to the controller; all data and ordinary command replies are portable.
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use wt_runtime::SourceState;
use wt_tui::{Board, UiAction, UiReply};

pub const HOST_PROTOCOL: u32 = 1;
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RowLayout {
    pub base_branch: Option<String>,
    pub work: Option<wt_core::WorkStatusRecord>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HostSnapshot {
    pub board: Option<Board>,
    pub state: HostState,
    /// Only facts used in cross-host layout. Controller sections and ordering
    /// stay in its own database, independent of the worker's personal layout.
    pub layout: BTreeMap<String, RowLayout>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HostState {
    Empty,
    Refreshing,
    Ready,
    Failed(String),
}

impl From<&SourceState> for HostState {
    fn from(state: &SourceState) -> Self {
        match state {
            SourceState::Empty => Self::Empty,
            SourceState::Refreshing => Self::Refreshing,
            SourceState::Ready => Self::Ready,
            SourceState::Failed(error) => Self::Failed(error.to_string()),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct HostViews {
    pub history: bool,
    pub perf: bool,
    pub perf_continuous: bool,
    pub perf_revision: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ClientFrame {
    Hello {
        protocol: u32,
    },
    Refresh,
    /// Read subscriptions are idempotent connection state, never mutations.
    Views(HostViews),
    Command {
        id: u64,
        action: UiAction,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ServerFrame {
    Hello { protocol: u32, build: String },
    Snapshot(HostSnapshot),
    Reply { id: u64, reply: UiReply },
}

impl HostSnapshot {
    pub fn capture(host: &crate::host_service::HostService) -> Self {
        let snapshot = host.sources.board.snapshot();
        let metadata = host.sources.metadata.snapshot();
        let mut layout = BTreeMap::new();
        if let Some(board) = &snapshot.data {
            for row in &board.rows {
                let entry = metadata
                    .data
                    .as_ref()
                    .map(|data| &data.0["slugs"][&row.key]);
                layout.insert(
                    row.key.clone(),
                    RowLayout {
                        base_branch: entry
                            .and_then(|e| e["baseBranch"].as_str())
                            .map(str::to_owned),
                        work: entry.and_then(|e| wt_core::parse_work_status(&e["work"])),
                    },
                );
            }
        }
        Self {
            board: snapshot.data.as_deref().cloned(),
            state: (&snapshot.state).into(),
            layout,
        }
    }

    pub fn validate(&self) -> Result<()> {
        if let Some(board) = &self.board {
            let mut seen = std::collections::BTreeSet::new();
            for row in &board.rows {
                if row.key.is_empty()
                    || row.key.starts_with('@')
                    || row.key.chars().any(char::is_control)
                    || !seen.insert(&row.key)
                {
                    bail!(
                        "host snapshot has invalid or duplicate local row identity {:?}",
                        row.key
                    );
                }
            }
            if self.layout.keys().any(|key| !seen.contains(key)) {
                bail!("host snapshot contains layout for an absent row");
            }
            let mut removed = std::collections::BTreeSet::new();
            let mut reviews = std::collections::BTreeSet::new();
            for review in &board.review_requests {
                if review.number == 0
                    || review.url.is_empty()
                    || review.branch.is_empty()
                    || [&review.url, &review.branch, &review.updated_at]
                        .iter()
                        .any(|value| value.len() > 4096 || value.chars().any(char::is_control))
                    || !reviews.insert(&review.url)
                {
                    bail!("host snapshot contains an invalid or duplicate review request");
                }
            }
            for row in &board.removed_history.rows {
                if row.key.is_empty()
                    || row.key.starts_with('@')
                    || row.key.chars().any(char::is_control)
                    || !removed.insert(&row.key)
                {
                    bail!(
                        "host snapshot has invalid or duplicate removed row identity {:?}",
                        row.key
                    );
                }
            }
        } else if !self.layout.is_empty() {
            bail!("host snapshot has layout without a board");
        }
        Ok(())
    }
}

pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(value)?;
    if bytes.len() >= MAX_FRAME_BYTES {
        bail!("host protocol frame exceeds 8 MiB");
    }
    bytes.push(b'\n');
    Ok(bytes)
}

/// `read_until` alone can allocate without bound on a damaged SSH stream.
/// Read through `take` before deserializing and reject truncated frames.
pub async fn read<T: serde::de::DeserializeOwned, R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
) -> Result<Option<T>> {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt};
    let mut bytes = Vec::new();
    let length = reader
        .take((MAX_FRAME_BYTES + 1) as u64)
        .read_until(b'\n', &mut bytes)
        .await?;
    if length == 0 {
        return Ok(None);
    }
    if length > MAX_FRAME_BYTES || bytes.last() != Some(&b'\n') {
        bail!("truncated or oversized host protocol frame");
    }
    Ok(Some(serde_json::from_slice(&bytes)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn fragmented_frames_are_complete_and_truncation_is_an_error() {
        use tokio::io::{AsyncWriteExt, BufReader};
        let (mut writer, reader) = tokio::io::duplex(16);
        let task = tokio::spawn(async move {
            for byte in encode(&ClientFrame::Command {
                id: 1,
                action: UiAction::Create {
                    input: "quoted task".into(),
                },
            })
            .unwrap()
            {
                writer.write_all(&[byte]).await.unwrap();
            }
            writer.write_all(b"{\"Hello\":").await.unwrap();
        });
        let mut reader = BufReader::new(reader);
        assert!(matches!(
            read::<ClientFrame, _>(&mut reader).await.unwrap(),
            Some(ClientFrame::Command { id: 1, .. })
        ));
        assert!(read::<ClientFrame, _>(&mut reader).await.is_err());
        task.await.unwrap();
    }
    #[test]
    fn duplicate_or_remote_keys_reject_the_entire_snapshot() {
        let row = wt_tui::BoardRow {
            key: "one".into(),
            ..Default::default()
        };
        let mut snapshot = HostSnapshot {
            board: Some(Board {
                rows: vec![row.clone(), row],
                ..Default::default()
            }),
            state: HostState::Ready,
            layout: BTreeMap::new(),
        };
        assert!(snapshot.validate().is_err());
        snapshot.board.as_mut().unwrap().rows.pop();
        snapshot.validate().unwrap();
        snapshot.board.as_mut().unwrap().rows[0].key = "@remote/other/one".into();
        assert!(snapshot.validate().is_err());
    }
}
