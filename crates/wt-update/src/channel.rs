use serde::{Deserialize, Serialize};

use crate::ArtifactError;

/// Release stream selected by a user or an isolated update test.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    #[default]
    Stable,
    Preview,
}

/// GitHub repository configuration for the release source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseRepository {
    owner: String,
    name: String,
    /// Optional API root for local test servers. Production uses api.github.com.
    api_base: String,
}

impl ReleaseRepository {
    pub fn new(owner: impl Into<String>, name: impl Into<String>) -> Result<Self, ArtifactError> {
        let owner = owner.into();
        let name = name.into();
        if !crate::validate_component(&owner) || !crate::validate_component(&name) {
            return Err(ArtifactError::InvalidRepository);
        }
        Ok(Self {
            owner,
            name,
            api_base: "https://api.github.com".into(),
        })
    }

    /// Override the API root for tests. Production callers should use `new`.
    pub fn with_api_base(mut self, base: impl Into<String>) -> Result<Self, ArtifactError> {
        let base = base.into();
        let url = reqwest::Url::parse(&base).map_err(|_| ArtifactError::InvalidApiBase)?;
        let host = url.host_str().unwrap_or_default();
        let production = url.scheme() == "https" && host == "api.github.com";
        let local_fixture =
            url.scheme() == "http" && matches!(host, "127.0.0.1" | "localhost" | "[::1]");
        if !(production || local_fixture) {
            return Err(ArtifactError::InvalidApiBase);
        }
        self.api_base = base.trim_end_matches('/').to_owned();
        Ok(self)
    }

    pub(crate) fn api_base(&self) -> &str {
        &self.api_base
    }

    pub fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }

    pub fn owner(&self) -> &str {
        &self.owner
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_slug_is_validated_as_two_path_components() {
        assert!(ReleaseRepository::new("micthiesen", "wt").is_ok());
        assert!(ReleaseRepository::new("../escape", "wt").is_err());
        assert!(ReleaseRepository::new("owner", "../repo").is_err());
    }
}
