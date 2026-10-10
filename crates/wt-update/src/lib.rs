//! Native update metadata, immutable release staging, and durable install state.
//!
//! The updater never writes into the source checkout. It accepts explicit
//! install paths and release configuration so tests can use isolated roots and
//! a local GitHub API fixture.

mod artifact;
mod channel;
mod engine;
mod installer;
mod paths;
mod state;
mod store;

pub use artifact::{
    ArtifactError, BuildInfo, ExtractedRelease, ReleaseAsset, ReleaseInfo, ReleaseSource,
    VerifiedRelease, artifact_name, parse_sha256, safe_extract_release, sha256_hex,
    verify_checksum,
};
pub use channel::{Channel, ReleaseRepository};
pub use engine::{BootTransitionError, InstallError, InstallManager};
pub use installer::{InstallOutcome, InstallerError, activate_existing, install_verified};
pub use paths::{InstallPaths, PathError, VersionId, validate_component};
pub use state::{
    InstallState, PendingBoot, StateError, StateHistoryEntry, UpdateOffer, update_check_due,
    update_offer,
};
pub use store::{StateLock, StateStore, StoreError, atomic_write};
