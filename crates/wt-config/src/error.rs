use std::path::PathBuf;

/// Configuration loading failed. Validation errors are collected so one run
/// reports every missing or malformed field that can be identified safely.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("configuration file not found at {path}")]
    NotFound { path: PathBuf },
    #[error("failed to read configuration at {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse {path}: {message}")]
    Parse { path: PathBuf, message: String },
    #[error("invalid config at {path}:\n{errors}")]
    Invalid { path: PathBuf, errors: String },
}

impl ConfigError {
    pub(crate) fn invalid(path: PathBuf, messages: Vec<String>) -> Self {
        Self::Invalid {
            path,
            errors: messages
                .into_iter()
                .map(|message| format!("  - {message}"))
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }
}
