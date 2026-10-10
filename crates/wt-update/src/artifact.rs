use std::{
    collections::BTreeMap,
    io::{Cursor, Read},
    time::Duration,
};

use flate2::read::GzDecoder;
use reqwest::{Client, StatusCode, Url, header};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{Channel, ReleaseRepository, VersionId, validate_component};

pub const UPDATER_USER_AGENT: &str = "OpenAI File Downloader, XaiImageApiFetch/1.0";
pub const DEFAULT_MAX_METADATA_BYTES: usize = 2 * 1024 * 1024;
pub const DEFAULT_MAX_ARCHIVE_BYTES: usize = 256 * 1024 * 1024;
const MAX_MEMBER_BYTES: u64 = 128 * 1024 * 1024;
const MAX_TOTAL_BINARY_BYTES: u64 = 192 * 1024 * 1024;
const MAX_ARCHIVE_ENTRIES: usize = 16;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseAsset {
    name: String,
    download_url: Url,
    size: u64,
    sha256: String,
}

impl ReleaseAsset {
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn size(&self) -> u64 {
        self.size
    }
    pub fn sha256(&self) -> &str {
        &self.sha256
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseInfo {
    tag: String,
    build_id: String,
    title: String,
    channel: Channel,
    prerelease: bool,
    published_at: Option<String>,
    assets: BTreeMap<String, ReleaseAsset>,
}

impl ReleaseInfo {
    pub fn version_id(&self, target: &str) -> Result<VersionId, ArtifactError> {
        VersionId::new(self.tag.clone(), self.build_id.clone(), target)
            .map_err(|_| ArtifactError::InvalidBuildId)
    }

    pub fn asset(&self, name: &str) -> Option<&ReleaseAsset> {
        self.assets.get(name)
    }

    pub fn tag(&self) -> &str {
        &self.tag
    }
    pub fn build_id(&self) -> &str {
        &self.build_id
    }
    pub fn title(&self) -> &str {
        &self.title
    }
    pub fn channel(&self) -> Channel {
        self.channel
    }
    pub fn prerelease(&self) -> bool {
        self.prerelease
    }
    pub fn published_at(&self) -> Option<&str> {
        self.published_at.as_deref()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedRelease {
    pub(crate) version: VersionId,
    pub(crate) app_binary: Vec<u8>,
    pub(crate) launcher_binary: Vec<u8>,
    pub(crate) archive_sha256: String,
}

impl VerifiedRelease {
    pub fn version(&self) -> &VersionId {
        &self.version
    }

    pub fn archive_sha256(&self) -> &str {
        &self.archive_sha256
    }

    /// Verified application bytes, for provisioning a worker of another target.
    /// The caller must preserve the release version and probe on that worker.
    pub fn app_binary(&self) -> &[u8] {
        &self.app_binary
    }
}

#[derive(Clone)]
pub struct ReleaseSource {
    repository: ReleaseRepository,
    client: Client,
    max_metadata_bytes: usize,
    max_archive_bytes: usize,
}

impl ReleaseSource {
    pub fn new(repository: ReleaseRepository) -> Result<Self, ArtifactError> {
        if rustls::crypto::CryptoProvider::get_default().is_none() {
            let _ = rustls::crypto::ring::default_provider().install_default();
        }
        let client = Client::builder()
            .user_agent(UPDATER_USER_AGENT)
            .timeout(Duration::from_secs(90))
            .connect_timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                let Some(host) = attempt.url().host_str() else {
                    return attempt.stop();
                };
                if host == "github.com"
                    || host == "api.github.com"
                    || host == "release-assets.githubusercontent.com"
                    || host.ends_with(".githubusercontent.com")
                    || host == "127.0.0.1"
                    || host == "localhost"
                {
                    attempt.follow()
                } else {
                    attempt.stop()
                }
            }))
            .build()
            .map_err(|error| ArtifactError::Transport(error.to_string()))?;
        Ok(Self {
            repository,
            client,
            max_metadata_bytes: DEFAULT_MAX_METADATA_BYTES,
            max_archive_bytes: DEFAULT_MAX_ARCHIVE_BYTES,
        })
    }

    /// Bounds are configurable for fixtures and unusually large future builds.
    pub fn with_limits(mut self, metadata: usize, archive: usize) -> Result<Self, ArtifactError> {
        if metadata == 0 || archive == 0 {
            return Err(ArtifactError::InvalidLimits);
        }
        self.max_metadata_bytes = metadata;
        self.max_archive_bytes = archive;
        Ok(self)
    }

    pub async fn latest(&self, channel: Channel) -> Result<ReleaseInfo, ArtifactError> {
        let base = self.repository.api_base();
        let url = match channel {
            Channel::Stable => format!(
                "{base}/repos/{}/{}/releases/latest",
                self.repository.owner(),
                self.repository.name()
            ),
            Channel::Preview => format!(
                "{base}/repos/{}/{}/releases?per_page=100",
                self.repository.owner(),
                self.repository.name()
            ),
        };
        let bytes = self.fetch_bounded(&url, self.max_metadata_bytes).await?;
        let release = match channel {
            Channel::Stable => {
                let value: ApiRelease = serde_json::from_slice(&bytes)
                    .map_err(|error| ArtifactError::InvalidMetadata(error.to_string()))?;
                if value.draft || value.prerelease {
                    return Err(ArtifactError::ChannelMismatch);
                }
                value
            }
            Channel::Preview => {
                let releases: Vec<ApiRelease> = serde_json::from_slice(&bytes)
                    .map_err(|error| ArtifactError::InvalidMetadata(error.to_string()))?;
                newest_preview(releases).ok_or(ArtifactError::NoRelease(channel))?
            }
        };
        self.load_manifest(release, channel).await
    }

    /// Select a release by an explicitly supplied GitHub tag. This path is
    /// intentionally separate from channel discovery so manual test tags are
    /// never considered by automatic preview updates.
    pub async fn by_tag(&self, tag: &str) -> Result<ReleaseInfo, ArtifactError> {
        if !validate_component(tag) {
            return Err(ArtifactError::InvalidBuildId);
        }
        let url = format!(
            "{}/repos/{}/{}/releases/tags/{}",
            self.repository.api_base(),
            self.repository.owner(),
            self.repository.name(),
            tag
        );
        let bytes = self.fetch_bounded(&url, self.max_metadata_bytes).await?;
        let release: ApiRelease = serde_json::from_slice(&bytes)
            .map_err(|error| ArtifactError::InvalidMetadata(error.to_string()))?;
        if release.draft || release.tag_name != tag {
            return Err(ArtifactError::ChannelMismatch);
        }
        let channel = if release.prerelease {
            Channel::Preview
        } else {
            Channel::Stable
        };
        self.load_manifest(release, channel).await
    }

    async fn load_manifest(
        &self,
        release: ApiRelease,
        channel: Channel,
    ) -> Result<ReleaseInfo, ArtifactError> {
        let tag = release.tag_name;
        if !validate_component(&tag) {
            return Err(ArtifactError::InvalidBuildId);
        }
        let mut github_assets = BTreeMap::new();
        for asset in release.assets {
            if !validate_asset_name(&asset.name) {
                continue;
            }
            let download_url = Url::parse(&asset.browser_download_url)
                .map_err(|_| ArtifactError::InvalidAssetUrl(asset.name.clone()))?;
            self.validate_url(&download_url)?;
            if github_assets.contains_key(&asset.name) {
                return Err(ArtifactError::InvalidMetadata(format!(
                    "duplicate asset {}",
                    asset.name
                )));
            }
            github_assets.insert(
                asset.name.clone(),
                ReleaseAsset {
                    name: asset.name,
                    download_url,
                    size: asset.size,
                    sha256: String::new(),
                },
            );
        }
        let manifest_asset = github_assets
            .get("wt-release.json")
            .ok_or_else(|| ArtifactError::MissingAsset("wt-release.json".into()))?;
        let manifest_bytes = self
            .fetch_bounded(
                manifest_asset.download_url.as_str(),
                self.max_metadata_bytes,
            )
            .await?;
        let manifest: ReleaseManifest = serde_json::from_slice(&manifest_bytes)
            .map_err(|error| ArtifactError::InvalidMetadata(error.to_string()))?;
        if manifest.schema_version != 1 || manifest.release_version != tag {
            return Err(ArtifactError::InvalidMetadata(
                "release manifest identity mismatch".into(),
            ));
        }
        if !is_full_build_id(&manifest.build_id) {
            return Err(ArtifactError::InvalidBuildId);
        }
        let mut assets = BTreeMap::new();
        for artifact in manifest.artifacts {
            if !validate_component(&artifact.target)
                || artifact.sha256.len() != 64
                || !artifact.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
                || artifact.size == 0
                || artifact.size > self.max_archive_bytes as u64
            {
                return Err(ArtifactError::InvalidMetadata(
                    "invalid artifact record".into(),
                ));
            }
            let version = VersionId::new(
                tag.clone(),
                manifest.build_id.clone(),
                artifact.target.clone(),
            )
            .map_err(|_| ArtifactError::InvalidBuildId)?;
            let expected_name = artifact_name(&version);
            if artifact.filename != expected_name {
                return Err(ArtifactError::InvalidMetadata(
                    "artifact filename does not match identity".into(),
                ));
            }
            let mut asset = github_assets
                .get(&artifact.filename)
                .cloned()
                .ok_or_else(|| ArtifactError::MissingAsset(artifact.filename.clone()))?;
            if asset.size != artifact.size {
                return Err(ArtifactError::InvalidMetadata(
                    "asset size does not match release manifest".into(),
                ));
            }
            asset.sha256 = artifact.sha256.to_ascii_lowercase();
            if assets.insert(asset.name.clone(), asset).is_some() {
                return Err(ArtifactError::InvalidMetadata(
                    "duplicate artifact target".into(),
                ));
            }
        }
        if assets.is_empty() {
            return Err(ArtifactError::InvalidMetadata(
                "release manifest has no artifacts".into(),
            ));
        }
        Ok(ReleaseInfo {
            tag,
            build_id: manifest.build_id,
            title: release.name.unwrap_or_default(),
            channel,
            prerelease: release.prerelease,
            published_at: release.published_at,
            assets,
        })
    }

    pub async fn download_verified(
        &self,
        release: &ReleaseInfo,
        target: &str,
    ) -> Result<VerifiedRelease, ArtifactError> {
        let version = release.version_id(target)?;
        let archive_name = artifact_name(&version);
        let archive = release
            .asset(&archive_name)
            .ok_or_else(|| ArtifactError::MissingAsset(archive_name.clone()))?;
        if archive.size > self.max_archive_bytes as u64 {
            return Err(ArtifactError::TooLarge {
                actual: archive.size,
                limit: self.max_archive_bytes as u64,
            });
        }
        let expected_hash = archive.sha256.clone();
        let bytes = self
            .fetch_bounded(archive.download_url.as_ref(), self.max_archive_bytes)
            .await?;
        let actual_hash = sha256_hex(&bytes);
        if actual_hash != expected_hash {
            return Err(ArtifactError::ChecksumMismatch {
                expected: expected_hash,
                actual: actual_hash,
            });
        }
        let extracted = safe_extract_release(&bytes, &archive_name)?;
        if extracted.build_info.release_version != release.tag
            || extracted.build_info.build_id != release.build_id
            || extracted.build_info.target != target
        {
            return Err(ArtifactError::InvalidMetadata(
                "archive build identity mismatch".into(),
            ));
        }
        Ok(VerifiedRelease {
            version,
            app_binary: extracted.app_binary,
            launcher_binary: extracted.launcher_binary,
            archive_sha256: actual_hash,
        })
    }

    async fn fetch_bounded(&self, url: &str, limit: usize) -> Result<Vec<u8>, ArtifactError> {
        let url = Url::parse(url).map_err(|_| ArtifactError::InvalidAssetUrl(url.into()))?;
        self.validate_url(&url)?;
        let response = self
            .client
            .get(url)
            .header(header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await
            .map_err(|error| ArtifactError::Transport(error.to_string()))?;
        if !response.status().is_success() {
            return Err(ArtifactError::HttpStatus(response.status()));
        }
        if response
            .content_length()
            .is_some_and(|length| length > limit as u64)
        {
            return Err(ArtifactError::TooLarge {
                actual: response.content_length().unwrap_or_default(),
                limit: limit as u64,
            });
        }
        let mut bytes =
            Vec::with_capacity(response.content_length().unwrap_or(0).min(limit as u64) as usize);
        let mut stream = response.bytes_stream();
        use futures_util::StreamExt;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| ArtifactError::Transport(error.to_string()))?;
            if bytes.len().saturating_add(chunk.len()) > limit {
                return Err(ArtifactError::TooLarge {
                    actual: bytes.len().saturating_add(chunk.len()) as u64,
                    limit: limit as u64,
                });
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }

    fn validate_url(&self, url: &Url) -> Result<(), ArtifactError> {
        if url.scheme() != "https"
            && !(url.scheme() == "http"
                && matches!(url.host_str(), Some("127.0.0.1" | "localhost")))
        {
            return Err(ArtifactError::InsecureUrl);
        }
        let api_host = Url::parse(self.repository.api_base())
            .ok()
            .and_then(|value| value.host_str().map(str::to_owned));
        let host = url.host_str().unwrap_or_default();
        if host != "github.com"
            && host != "api.github.com"
            && host != "release-assets.githubusercontent.com"
            && !host.ends_with(".githubusercontent.com")
            && Some(host.to_owned()) != api_host
        {
            return Err(ArtifactError::UntrustedHost(host.to_owned()));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
struct ReleaseManifest {
    schema_version: u32,
    release_version: String,
    build_id: String,
    artifacts: Vec<ManifestArtifact>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
struct ManifestArtifact {
    target: String,
    filename: String,
    sha256: String,
    size: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct BuildInfo {
    pub schema_version: u32,
    pub release_version: String,
    pub build_id: String,
    pub target: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtractedRelease {
    pub app_binary: Vec<u8>,
    pub launcher_binary: Vec<u8>,
    pub build_info: BuildInfo,
}

/// The payload is extracted only from the release's three fixed paths.
pub fn safe_extract_release(
    archive_bytes: &[u8],
    archive_name: &str,
) -> Result<ExtractedRelease, ArtifactError> {
    if archive_bytes.len() > DEFAULT_MAX_ARCHIVE_BYTES {
        return Err(ArtifactError::TooLarge {
            actual: archive_bytes.len() as u64,
            limit: DEFAULT_MAX_ARCHIVE_BYTES as u64,
        });
    }
    let stem = archive_name
        .strip_suffix(".tar.gz")
        .ok_or(ArtifactError::InvalidArchiveName)?;
    if !validate_component(stem) {
        return Err(ArtifactError::InvalidArchiveName);
    }
    let expected_app = format!("{stem}/wt");
    let expected_launcher = format!("{stem}/wt-launcher");
    let expected_info = format!("{stem}/wt-build-info.json");
    let expected_root = format!("{stem}/");
    let decoder = GzDecoder::new(Cursor::new(archive_bytes));
    let mut archive = tar::Archive::new(decoder);
    let entries = archive
        .entries()
        .map_err(|error| ArtifactError::InvalidArchive(error.to_string()))?;
    let mut app = None;
    let mut launcher = None;
    let mut build_info = None;
    let mut total = 0_u64;
    for (entry_index, entry) in entries.enumerate() {
        if entry_index >= MAX_ARCHIVE_ENTRIES {
            return Err(ArtifactError::TooManyArchiveEntries);
        }
        let entry = entry.map_err(|error| ArtifactError::InvalidArchive(error.to_string()))?;
        let path = entry
            .path()
            .map_err(|error| ArtifactError::InvalidArchive(error.to_string()))?;
        let path = path.to_str().ok_or(ArtifactError::InvalidArchivePath)?;
        if path == expected_root || path == stem {
            if !entry.header().entry_type().is_dir() {
                return Err(ArtifactError::InvalidArchivePath);
            }
            continue;
        }
        let kind = if path == expected_app {
            0
        } else if path == expected_launcher {
            1
        } else if path == expected_info {
            2
        } else {
            return Err(ArtifactError::UnexpectedArchiveEntry(path.to_owned()));
        };
        if !entry.header().entry_type().is_file()
            || (kind == 0 && app.is_some())
            || (kind == 1 && launcher.is_some())
            || (kind == 2 && build_info.is_some())
        {
            return Err(ArtifactError::InvalidArchivePath);
        }
        let size = entry
            .header()
            .size()
            .map_err(|error| ArtifactError::InvalidArchive(error.to_string()))?;
        let member_limit = if kind == 2 {
            16 * 1024
        } else {
            MAX_MEMBER_BYTES
        };
        if size > member_limit || total.saturating_add(size) > MAX_TOTAL_BINARY_BYTES {
            return Err(ArtifactError::ExpandedTooLarge);
        }
        let mut bytes = Vec::with_capacity(size as usize);
        entry
            .take(MAX_MEMBER_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| ArtifactError::InvalidArchive(error.to_string()))?;
        if bytes.len() as u64 != size {
            return Err(ArtifactError::InvalidArchive("truncated file entry".into()));
        }
        total += size;
        match kind {
            0 => app = Some(bytes),
            1 => launcher = Some(bytes),
            _ => {
                let info: BuildInfo = serde_json::from_slice(&bytes)
                    .map_err(|error| ArtifactError::InvalidMetadata(error.to_string()))?;
                if info.schema_version != 1
                    || !validate_component(&info.release_version)
                    || !is_full_build_id(&info.build_id)
                    || !validate_component(&info.target)
                {
                    return Err(ArtifactError::InvalidMetadata(
                        "invalid archive build info".into(),
                    ));
                }
                build_info = Some(info);
            }
        }
    }
    let app = app.ok_or(ArtifactError::MissingArchiveBinary("wt"))?;
    let launcher = launcher.ok_or(ArtifactError::MissingArchiveBinary("wt-launcher"))?;
    Ok(ExtractedRelease {
        app_binary: app,
        launcher_binary: launcher,
        build_info: build_info.ok_or(ArtifactError::MissingArchiveBinary("wt-build-info.json"))?,
    })
}

pub fn artifact_name(version: &VersionId) -> String {
    format!(
        "wt-{}-{}.tar.gz",
        version.release_version(),
        version.target()
    )
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Read a conventional `sha256sum` line while binding it to the expected asset.
pub fn parse_sha256(text: &str, expected_file: &str) -> Result<String, ArtifactError> {
    if !validate_asset_name(expected_file) {
        return Err(ArtifactError::InvalidChecksum);
    }
    let lines: Vec<_> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    if lines.len() != 1 {
        return Err(ArtifactError::InvalidChecksum);
    }
    let mut fields = lines[0].split_whitespace();
    let hash = fields.next().ok_or(ArtifactError::InvalidChecksum)?;
    let file = fields.next().ok_or(ArtifactError::InvalidChecksum)?;
    if fields.next().is_some() || file.trim_start_matches('*') != expected_file {
        return Err(ArtifactError::InvalidChecksum);
    }
    if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(ArtifactError::InvalidChecksum);
    }
    Ok(hash.to_ascii_lowercase())
}

pub fn verify_checksum(bytes: &[u8], expected_hex: &str) -> Result<(), ArtifactError> {
    let expected = expected_hex.to_ascii_lowercase();
    if expected.len() != 64 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(ArtifactError::InvalidChecksum);
    }
    let actual = sha256_hex(bytes);
    if actual == expected {
        Ok(())
    } else {
        Err(ArtifactError::ChecksumMismatch { expected, actual })
    }
}

fn validate_asset_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() < 240
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._+-".contains(&byte))
}

fn is_full_build_id(build_id: &str) -> bool {
    matches!(build_id.len(), 40 | 64) && build_id.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn is_preview_tag(tag: &str) -> bool {
    tag.strip_prefix("preview-").is_some_and(is_full_build_id)
}

/// The most recently published preview. GitHub's release list is not in
/// publication order (it listed the first preview ahead of later ones), so
/// taking the first match pinned preview installs to an old build.
fn newest_preview(releases: Vec<ApiRelease>) -> Option<ApiRelease> {
    releases
        .into_iter()
        .filter(|release| !release.draft && is_preview_tag(&release.tag_name))
        .max_by(|a, b| a.published_at.cmp(&b.published_at))
}

#[derive(Deserialize)]
struct ApiRelease {
    tag_name: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    published_at: Option<String>,
    #[serde(default)]
    assets: Vec<ApiAsset>,
}

#[derive(Deserialize)]
struct ApiAsset {
    name: String,
    browser_download_url: String,
    size: u64,
}

#[derive(Debug, Error)]
pub enum ArtifactError {
    #[error("invalid GitHub repository owner/name")]
    InvalidRepository,
    #[error("invalid test API base URL")]
    InvalidApiBase,
    #[error("invalid release metadata: {0}")]
    InvalidMetadata(String),
    #[error("release metadata has an unsafe build id")]
    InvalidBuildId,
    #[error("GitHub release does not match the requested channel")]
    ChannelMismatch,
    #[error("no release is available on the {0:?} channel")]
    NoRelease(Channel),
    #[error("release asset is missing: {0}")]
    MissingAsset(String),
    #[error("release asset URL is invalid: {0}")]
    InvalidAssetUrl(String),
    #[error("release URL must use HTTPS")]
    InsecureUrl,
    #[error("release URL host is not trusted: {0}")]
    UntrustedHost(String),
    #[error("GitHub returned HTTP status {0}")]
    HttpStatus(StatusCode),
    #[error("release payload size {actual} exceeds limit {limit} bytes")]
    TooLarge { actual: u64, limit: u64 },
    #[error("download limit must be non-zero")]
    InvalidLimits,
    #[error("release checksum is malformed or names a different asset")]
    InvalidChecksum,
    #[error("release checksum mismatch: expected {expected}, got {actual}")]
    ChecksumMismatch { expected: String, actual: String },
    #[error("release archive name is invalid")]
    InvalidArchiveName,
    #[error("release archive is invalid: {0}")]
    InvalidArchive(String),
    #[error("release archive contains an unsafe path")]
    InvalidArchivePath,
    #[error("release archive contains an unexpected entry: {0}")]
    UnexpectedArchiveEntry(String),
    #[error("release archive expands beyond the binary size limit")]
    ExpandedTooLarge,
    #[error("release archive contains too many entries")]
    TooManyArchiveEntries,
    #[error("release archive omitted required binary {0}")]
    MissingArchiveBinary(&'static str),
    #[error("release transport failed: {0}")]
    Transport(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compression, write::GzEncoder};
    use std::io;
    use tar::{Builder, Header};

    const TEST_BUILD_ID: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn preview_selection_excludes_manual_test_tags_and_requires_full_sha() {
        assert!(is_preview_tag(&format!("preview-{TEST_BUILD_ID}")));
        assert!(!is_preview_tag("rust-test-0123456789ab-123"));
        assert!(!is_preview_tag("preview-deadbeef"));
        assert!(!is_preview_tag("v1.2.3"));
    }

    fn archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut gzip = GzEncoder::new(Vec::new(), Compression::fast());
        {
            let mut tar = Builder::new(&mut gzip);
            for (name, bytes) in entries {
                let mut header = Header::new_gnu();
                header.set_size(bytes.len() as u64);
                header.set_mode(0o755);
                header.set_cksum();
                tar.append_data(&mut header, name, *bytes).unwrap();
            }
            tar.finish().unwrap();
        }
        gzip.finish().unwrap()
    }

    #[test]
    fn checksum_is_bound_to_expected_single_asset() {
        let bytes = b"binary contents";
        let digest = sha256_hex(bytes);
        assert_eq!(
            parse_sha256(
                &format!("{digest}  wt-preview-x86_64-linux.tar.gz\n"),
                "wt-preview-x86_64-linux.tar.gz"
            )
            .unwrap(),
            digest
        );
        assert!(
            parse_sha256(
                &format!("{digest}  other.tar.gz"),
                "wt-preview-x86_64-linux.tar.gz"
            )
            .is_err()
        );
        assert!(parse_sha256(&format!("{digest}  one\n{digest}  two"), "one").is_err());
        assert!(verify_checksum(bytes, &sha256_hex(bytes)).is_ok());
        assert!(verify_checksum(bytes, &"0".repeat(64)).is_err());
    }

    #[test]
    fn extraction_accepts_only_the_two_expected_regular_binaries() {
        let name = "wt-v1.2.3-aarch64-apple-darwin.tar.gz";
        let bytes = archive(&[
            ("wt-v1.2.3-aarch64-apple-darwin/wt", b"app"),
            ("wt-v1.2.3-aarch64-apple-darwin/wt-launcher", b"launcher"),
            (
                "wt-v1.2.3-aarch64-apple-darwin/wt-build-info.json",
                br#"{"schema_version":1,"release_version":"v1.2.3","build_id":"0123456789abcdef0123456789abcdef01234567","target":"aarch64-apple-darwin"}"#,
            ),
        ]);
        let extracted = safe_extract_release(&bytes, name).unwrap();
        assert_eq!(extracted.app_binary, b"app");
        assert_eq!(extracted.launcher_binary, b"launcher");
        assert_eq!(extracted.build_info.build_id, TEST_BUILD_ID);

        let traversal = archive(&[("wt-v1.2.3-aarch64-apple-darwin/wt-launcher", b"launcher")]);
        assert!(safe_extract_release(&traversal, name).is_err());

        let mut gzip = GzEncoder::new(Vec::new(), Compression::fast());
        {
            let mut tar = Builder::new(&mut gzip);
            let mut header = Header::new_gnu();
            header.as_mut_bytes()[.."wt-v1.2.3-aarch64-apple-darwin/../evil".len()]
                .copy_from_slice(b"wt-v1.2.3-aarch64-apple-darwin/../evil");
            header.set_size(4);
            header.set_mode(0o755);
            header.set_cksum();
            tar.append(&header, &b"evil"[..]).unwrap();
            tar.finish().unwrap();
        }
        assert!(safe_extract_release(&gzip.finish().unwrap(), name).is_err());

        let extra = archive(&[
            ("wt-v1.2.3-aarch64-apple-darwin/wt", b"app"),
            ("wt-v1.2.3-aarch64-apple-darwin/wt-launcher", b"launcher"),
            ("wt-v1.2.3-aarch64-apple-darwin/extra", b"unexpected"),
        ]);
        assert!(safe_extract_release(&extra, name).is_err());
    }

    #[test]
    fn path_entry_types_are_not_extracted() {
        let mut gzip = GzEncoder::new(Vec::new(), Compression::fast());
        {
            let mut tar = Builder::new(&mut gzip);
            let mut header = Header::new_gnu();
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_link_name("../../outside").unwrap();
            header.set_size(0);
            header.set_cksum();
            tar.append_data(&mut header, "wt-v1.2.3-x86_64-linux/wt", io::empty())
                .unwrap();
            tar.finish().unwrap();
        }
        let bytes = gzip.finish().unwrap();
        assert!(safe_extract_release(&bytes, "wt-v1.2.3-x86_64-linux.tar.gz").is_err());
    }

    #[test]
    fn names_are_stable_per_build_and_target() {
        let version = VersionId::new("v1.2.3", TEST_BUILD_ID, "aarch64-apple-darwin").unwrap();
        assert_eq!(
            artifact_name(&version),
            "wt-v1.2.3-aarch64-apple-darwin.tar.gz"
        );
    }

    #[tokio::test]
    async fn latest_reads_manifest_and_sends_required_user_agent() {
        use tokio::{
            io::{AsyncReadExt, AsyncWriteExt},
            net::TcpListener,
        };

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let archive_bytes = archive(&[
            ("wt-v1.2.3-x86_64-unknown-linux-gnu/wt", b"app"),
            ("wt-v1.2.3-x86_64-unknown-linux-gnu/wt-launcher", b"launcher"),
            (
                "wt-v1.2.3-x86_64-unknown-linux-gnu/wt-build-info.json",
                br#"{"schema_version":1,"release_version":"v1.2.3","build_id":"0123456789abcdef0123456789abcdef01234567","target":"x86_64-unknown-linux-gnu"}"#,
            ),
        ]);
        let manifest = serde_json::json!({
            "schema_version": 1,
            "release_version": "v1.2.3",
            "build_id": TEST_BUILD_ID,
            "artifacts": [{
                "target": "x86_64-unknown-linux-gnu",
                "filename": "wt-v1.2.3-x86_64-unknown-linux-gnu.tar.gz",
                "sha256": sha256_hex(&archive_bytes),
                "size": archive_bytes.len()
            }]
        })
        .to_string();
        let release = serde_json::json!({
            "tag_name": "v1.2.3", "name": "v1.2.3", "draft": false,
            "prerelease": false, "assets": [
                {"name":"wt-release.json", "browser_download_url":format!("{base}/manifest"), "size":manifest.len()},
                {"name":"wt-v1.2.3-x86_64-unknown-linux-gnu.tar.gz", "browser_download_url":format!("{base}/archive"), "size":archive_bytes.len()}
            ]
        }).to_string();
        let seen = tokio::spawn(async move {
            let mut user_agents = Vec::new();
            for (body, path, content_type) in [
                (release.into_bytes(), "/releases/latest", "application/json"),
                (manifest.into_bytes(), "/manifest", "application/json"),
                (archive_bytes, "/archive", "application/octet-stream"),
            ] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut buffer = [0; 1024];
                loop {
                    let count = stream.read(&mut buffer).await.unwrap();
                    request.extend_from_slice(&buffer[..count]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                let text = String::from_utf8(request).unwrap();
                assert!(
                    text.starts_with("GET ") && text.contains(path),
                    "unexpected request: {text}"
                );
                let agent = text
                    .lines()
                    .find(|line| line.to_ascii_lowercase().starts_with("user-agent:"))
                    .unwrap()
                    .split_once(':')
                    .unwrap()
                    .1
                    .trim()
                    .to_owned();
                user_agents.push(agent);
                let headers = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(headers.as_bytes()).await.unwrap();
                stream.write_all(&body).await.unwrap();
            }
            user_agents
        });
        let repository = ReleaseRepository::new("micthiesen", "wt")
            .unwrap()
            .with_api_base(base)
            .unwrap();
        let source = ReleaseSource::new(repository).unwrap();
        let release = source.latest(Channel::Stable).await.unwrap();
        assert_eq!(release.tag(), "v1.2.3");
        assert_eq!(release.build_id(), TEST_BUILD_ID);
        assert_eq!(
            release
                .version_id("x86_64-unknown-linux-gnu")
                .unwrap()
                .build_id(),
            TEST_BUILD_ID
        );
        let verified = source
            .download_verified(&release, "x86_64-unknown-linux-gnu")
            .await
            .unwrap();
        assert_eq!(verified.app_binary, b"app");
        assert_eq!(verified.launcher_binary, b"launcher");
        assert_eq!(
            seen.await.unwrap(),
            vec![UPDATER_USER_AGENT, UPDATER_USER_AGENT, UPDATER_USER_AGENT]
        );
    }

    #[test]
    fn preview_discovery_takes_the_newest_publication_not_the_first_listed() {
        let release = |tag: &str, published: &str, draft: bool| ApiRelease {
            tag_name: tag.into(),
            name: None,
            draft,
            prerelease: true,
            published_at: Some(published.into()),
            assets: Vec::new(),
        };
        let sha = |c: char| c.to_string().repeat(40);
        let releases = vec![
            release(&format!("preview-{}", sha('a')), "2026-10-10T15:36:54Z", false),
            release(&format!("preview-{}", sha('b')), "2026-10-10T17:49:34Z", false),
            release(&format!("preview-{}", sha('c')), "2026-10-10T18:00:00Z", true),
            release("rust-test-0123456789ab-1", "2026-10-11T00:00:00Z", false),
        ];
        assert_eq!(
            newest_preview(releases).unwrap().tag_name,
            format!("preview-{}", sha('b'))
        );
    }

    #[tokio::test]
    async fn by_tag_can_select_manual_test_release_without_auto_offering_it() {
        use tokio::{
            io::{AsyncReadExt, AsyncWriteExt},
            net::TcpListener,
        };

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let tag = "rust-test-0123456789ab-123";
        let release = serde_json::json!({
            "tag_name": tag,
            "draft": false,
            "prerelease": true,
            "assets": []
        })
        .to_string();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 1024];
            loop {
                let count = stream.read(&mut buffer).await.unwrap();
                request.extend_from_slice(&buffer[..count]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let request = String::from_utf8(request).unwrap();
            assert!(request.starts_with("GET /repos/micthiesen/wt/releases/tags/rust-test-"));
            let body = release.into_bytes();
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(headers.as_bytes()).await.unwrap();
            stream.write_all(&body).await.unwrap();
        });
        let repository = ReleaseRepository::new("micthiesen", "wt")
            .unwrap()
            .with_api_base(base)
            .unwrap();
        let source = ReleaseSource::new(repository).unwrap();
        // The explicit selector accepts the test tag and parses it as preview;
        // manifest validation then fails because this fixture intentionally has
        // no assets. Automatic preview selection still rejects this tag.
        assert!(!is_preview_tag(tag));
        assert!(matches!(
            source.by_tag(tag).await,
            Err(ArtifactError::MissingAsset(_))
        ));
        server.await.unwrap();
    }
}
