//! Host-local bounded coordination holds. A hold only gates the operations
//! it names and is never sent anywhere by this command.

use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::context::AppContext;

const MAX_HOLD_MS: i128 = 60 * 60 * 1000;
const HOLD_FILE: &str = "communication-holds.json";

#[derive(Debug, Clone, Args)]
pub struct HoldArgs {
    #[command(subcommand)]
    pub command: HoldCommand,
}

#[derive(Debug, Clone, Subcommand)]
pub enum HoldCommand {
    Set {
        resource: String,
        #[arg(long)]
        scope: String,
        #[arg(long = "at")]
        event_at: String,
        #[arg(long)]
        until: String,
        #[arg(long)]
        owner: Option<String>,
        #[arg(trailing_var_arg = true, required = true)]
        reason: Vec<String>,
    },
    Release {
        resource: String,
        #[arg(long = "at")]
        event_at: String,
        #[arg(long)]
        owner: Option<String>,
        #[arg(trailing_var_arg = true, required = true)]
        reason: Vec<String>,
    },
    Check {
        id: String,
    },
    Status {
        resource: String,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Hold {
    pub id: String,
    pub resource: String,
    pub scope: String,
    pub owner: String,
    pub event_at: String,
    pub until: String,
    pub reason: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Release {
    pub resource: String,
    pub owner: String,
    pub event_at: String,
    pub reason: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
enum Event {
    Hold { hold: Hold },
    Release { release: Release },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct HoldStore {
    version: u8,
    events: Vec<Event>,
}

#[derive(Clone, Debug, Serialize)]
pub struct HoldCheck {
    pub active: bool,
    pub reason: String,
    pub hold: Option<Hold>,
}

pub async fn run(context: &AppContext, args: &HoldArgs) -> Result<i32> {
    let path = hold_path(context);
    match &args.command {
        HoldCommand::Set {
            resource,
            scope,
            event_at,
            until,
            owner,
            reason,
        } => {
            let owner = owner_for(owner.as_deref())?;
            let input = Hold {
                id: String::new(),
                resource: resource.clone(),
                scope: scope.clone(),
                owner,
                event_at: event_at.clone(),
                until: until.clone(),
                reason: reason.join(" ").trim().to_owned(),
            };
            let hold = with_store_lock(&path, |store| set_hold(store, input))?;
            println!("{}", serde_json::to_string(&hold)?);
        }
        HoldCommand::Release {
            resource,
            event_at,
            owner,
            reason,
        } => {
            let owner = owner_for(owner.as_deref())?;
            let release = Release {
                resource: resource.clone(),
                owner,
                event_at: event_at.clone(),
                reason: reason.join(" ").trim().to_owned(),
            };
            with_store_lock(&path, |store| release_hold(store, release))?;
            println!(
                "{}",
                serde_json::json!({"released": true, "resource": resource, "eventAt": canonical_timestamp(event_at)?})
            );
        }
        HoldCommand::Check { id } => println!(
            "{}",
            serde_json::to_string(&check_id(&load_store(&path)?, id)?)?
        ),
        HoldCommand::Status { resource } => println!(
            "{}",
            serde_json::to_string(&check_resource(&load_store(&path)?, resource)?)?
        ),
    }
    Ok(0)
}

pub fn hold_path(context: &AppContext) -> PathBuf {
    context
        .config
        .paths
        .state_db
        .parent()
        .unwrap_or(Path::new("."))
        .join(HOLD_FILE)
}

pub fn prepare_hold_message(path: &Path, id: &str, text: &str) -> Result<String> {
    let current = check_id(&load_store(path)?, id)?;
    let Some(hold) = current.hold.filter(|_| current.active) else {
        bail!("hold {id} was not sent: {}", current.reason);
    };
    let hold_json = serde_json::to_string(&hold)?;
    let mut lines = vec![
        "[wt transient resource hold reference]".to_owned(),
        hold_json,
        format!("Before acting, run: wt hold check {}", hold.id),
        "This queued copy is not current authority. Honor only active:true for this exact ID, scope and deadline; released, superseded, expired or unknown references impose no hold. A failed check does not authorize a new freeze.".to_owned(),
        "Only the named operations are affected. Continue independent code, tests, reviews and merges. No acknowledgment requested; forwarding or acknowledging this reference cannot renew it.".to_owned(),
    ];
    if !text.trim().is_empty() {
        lines.push(text.trim().to_owned());
    }
    Ok(lines.join("\n"))
}

fn owner_for(owner: Option<&str>) -> Result<String> {
    let owner = owner
        .map(str::to_owned)
        .or_else(|| std::env::var("WT_AGENT").ok());
    owner
        .filter(|value| valid_text(value))
        .context("owner is required (pass --owner outside an agent)")
}

fn valid_text(value: &str) -> bool {
    !value.trim().is_empty() && value.trim() == value && !value.chars().any(char::is_control)
}

fn validate_record(value: &str, label: &str) -> Result<String> {
    if !valid_text(value) {
        bail!("{label} must be a nonempty, trimmed, single-line string");
    }
    Ok(value.to_owned())
}

fn canonical_timestamp(value: &str) -> Result<String> {
    // Rfc3339 accepts only timestamps with an explicit timezone. Keep parity
    // with the TS contract's millisecond precision and canonical UTC output.
    let zone_start = if value.ends_with('Z') {
        value.len() - 1
    } else {
        value.len().saturating_sub(6)
    };
    let zone = &value[zone_start..];
    let explicit_zone = zone == "Z"
        || (zone.len() == 6
            && matches!(zone.as_bytes()[0], b'+' | b'-')
            && zone.as_bytes()[3] == b':'
            && zone.as_bytes()[1..3].iter().all(u8::is_ascii_digit)
            && zone.as_bytes()[4..6].iter().all(u8::is_ascii_digit));
    let date_time = &value[..zone_start];
    let precision_ok = match date_time.split_once('.') {
        Some((base, fraction)) => {
            base.len() == 19
                && (1..=3).contains(&fraction.len())
                && fraction.bytes().all(|b| b.is_ascii_digit())
        }
        None => date_time.len() == 19,
    };
    if !explicit_zone || !precision_ok {
        bail!("timestamp must be ISO 8601 with an explicit timezone");
    }
    let parsed = OffsetDateTime::parse(value, &Rfc3339).context("invalid ISO timestamp")?;
    let millis = parsed.unix_timestamp_nanos().div_euclid(1_000_000);
    let utc = OffsetDateTime::from_unix_timestamp_nanos(millis * 1_000_000)
        .context("invalid timestamp")?;
    Ok(format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        utc.year(),
        u8::from(utc.month()),
        utc.day(),
        utc.hour(),
        utc.minute(),
        utc.second(),
        utc.millisecond()
    ))
}

fn hold_id(hold: &Hold) -> Result<String> {
    let bytes = serde_json::to_vec(&[&hold.resource, &hold.owner, &hold.event_at])?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn active(hold: &Hold, now: i128) -> Result<bool> {
    Ok(canonical_timestamp(&hold.event_at)? == hold.event_at
        && OffsetDateTime::parse(&hold.event_at, &Rfc3339)?
            .unix_timestamp_nanos()
            .div_euclid(1_000_000)
            <= now
        && now
            < OffsetDateTime::parse(&hold.until, &Rfc3339)?
                .unix_timestamp_nanos()
                .div_euclid(1_000_000))
}

fn set_hold(store: &mut HoldStore, mut hold: Hold) -> Result<Hold> {
    hold.resource = validate_record(&hold.resource, "resource")?;
    hold.scope = validate_record(&hold.scope, "scope")?;
    hold.owner = validate_record(&hold.owner, "owner")?;
    hold.reason = validate_record(&hold.reason, "reason")?;
    hold.event_at = canonical_timestamp(&hold.event_at)?;
    hold.until = canonical_timestamp(&hold.until)?;
    let start = OffsetDateTime::parse(&hold.event_at, &Rfc3339)?
        .unix_timestamp_nanos()
        .div_euclid(1_000_000);
    let end = OffsetDateTime::parse(&hold.until, &Rfc3339)?
        .unix_timestamp_nanos()
        .div_euclid(1_000_000);
    if end <= start || end - start > MAX_HOLD_MS {
        bail!("until must be after eventAt and no more than one hour later");
    }
    if start > now_ms() {
        bail!("eventAt cannot be in the future");
    }
    hold.id = hold_id(&hold)?;
    if let Some(previous) = store
        .events
        .iter()
        .find(|event| event_resource(event) == hold.resource)
    {
        let previous_at = event_time(previous)?;
        if let Event::Hold { hold: old } = previous {
            if old == &hold {
                return Ok(old.clone());
            }
            if active(old, now_ms())? {
                bail!(
                    "{} is already held by {} until {}",
                    hold.resource,
                    old.owner,
                    old.until
                );
            }
        }
        if start <= previous_at {
            bail!(
                "eventAt must be newer than the latest event for {}",
                hold.resource
            );
        }
    }
    replace_event(store, Event::Hold { hold: hold.clone() });
    Ok(hold)
}

fn release_hold(store: &mut HoldStore, mut release: Release) -> Result<()> {
    release.resource = validate_record(&release.resource, "resource")?;
    release.owner = validate_record(&release.owner, "owner")?;
    release.reason = validate_record(&release.reason, "reason")?;
    release.event_at = canonical_timestamp(&release.event_at)?;
    let at = OffsetDateTime::parse(&release.event_at, &Rfc3339)?
        .unix_timestamp_nanos()
        .div_euclid(1_000_000);
    if at > now_ms() {
        bail!("eventAt cannot be in the future");
    }
    if let Some(previous) = store
        .events
        .iter()
        .find(|event| event_resource(event) == release.resource)
    {
        let previous_at = event_time(previous)?;
        if at < previous_at {
            bail!(
                "release is older than the latest event for {}",
                release.resource
            );
        }
        if let Event::Release { release: old } = previous
            && old.event_at == release.event_at
        {
            return Ok(());
        }
        if let Event::Hold { hold } = previous
            && active(hold, now_ms())?
            && release.owner != hold.owner
        {
            bail!(
                "{} is held by {}, not {}",
                release.resource,
                hold.owner,
                release.owner
            );
        }
    }
    replace_event(store, Event::Release { release });
    Ok(())
}

fn replace_event(store: &mut HoldStore, event: Event) {
    let resource = event_resource(&event).to_owned();
    store
        .events
        .retain(|previous| event_resource(previous) != resource);
    store.events.push(event);
}

fn event_resource(event: &Event) -> &str {
    match event {
        Event::Hold { hold } => &hold.resource,
        Event::Release { release } => &release.resource,
    }
}

fn event_time(event: &Event) -> Result<i128> {
    let value = match event {
        Event::Hold { hold } => &hold.event_at,
        Event::Release { release } => &release.event_at,
    };
    Ok(OffsetDateTime::parse(value, &Rfc3339)?
        .unix_timestamp_nanos()
        .div_euclid(1_000_000))
}

fn check_id(store: &HoldStore, id: &str) -> Result<HoldCheck> {
    if id.len() != 64
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        bail!("id must be a SHA-256 hold identifier");
    }
    let event = store
        .events
        .iter()
        .find(|event| matches!(event, Event::Hold { hold } if hold.id == id));
    check_event(event, now_ms())
}

fn check_resource(store: &HoldStore, resource: &str) -> Result<HoldCheck> {
    validate_record(resource, "resource")?;
    check_event(
        store
            .events
            .iter()
            .find(|event| event_resource(event) == resource),
        now_ms(),
    )
}

fn check_event(event: Option<&Event>, now: i128) -> Result<HoldCheck> {
    Ok(match event {
        None => HoldCheck {
            active: false,
            reason: "hold is unknown or superseded".into(),
            hold: None,
        },
        Some(Event::Release { release }) => HoldCheck {
            active: false,
            reason: format!("released at {}: {}", release.event_at, release.reason),
            hold: None,
        },
        Some(Event::Hold { hold }) => {
            let active_now = active(hold, now)?;
            HoldCheck {
                active: active_now,
                reason: if active_now {
                    hold.reason.clone()
                } else {
                    format!("hold is inactive; deadline {}", hold.until)
                },
                hold: Some(hold.clone()),
            }
        }
    })
}

fn load_store(path: &Path) -> Result<HoldStore> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(HoldStore {
                version: 1,
                events: Vec::new(),
            });
        }
        Err(error) => {
            return Err(error).with_context(|| format!("read hold state {}", path.display()));
        }
    };
    let mut bytes = Vec::new();
    file.take(2 * 1024 * 1024).read_to_end(&mut bytes)?;
    let store: HoldStore =
        serde_json::from_slice(&bytes).context("parse communication hold state")?;
    if store.version != 1 {
        bail!(
            "unsupported communication hold storage version {}",
            store.version
        );
    }
    let mut resources = std::collections::BTreeSet::new();
    for event in &store.events {
        if !resources.insert(event_resource(event)) {
            bail!("duplicate resource in communication hold storage");
        }
        if let Event::Hold { hold } = event
            && (hold.id != hold_id(hold)?
                || canonical_timestamp(&hold.event_at)? != hold.event_at
                || canonical_timestamp(&hold.until)? != hold.until)
        {
            bail!("invalid stored hold for {}", hold.resource);
        }
    }
    Ok(store)
}

fn with_store_lock<T>(
    path: &Path,
    operation: impl FnOnce(&mut HoldStore) -> Result<T>,
) -> Result<T> {
    let parent = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let lock_path = path.with_extension("json.lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)?;
    lock_file(&lock)?;
    let mut store = load_store(path)?;
    let result = operation(&mut store)?;
    write_store(path, &store)?;
    unlock_file(&lock);
    Ok(result)
}

fn write_store(path: &Path, store: &HoldStore) -> Result<()> {
    let parent = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let tmp = path.with_extension(format!("json.{}.{}.tmp", std::process::id(), now_ms()));
    let bytes = serde_json::to_vec_pretty(store)?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&tmp)?;
    file.write_all(&bytes)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    fs::rename(&tmp, path).context("replace communication hold state")?;
    Ok(())
}

#[cfg(unix)]
fn lock_file(file: &File) -> Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: flock operates on this owned file descriptor and does not retain it.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == -1 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(unix)]
fn unlock_file(file: &File) {
    use std::os::fd::AsRawFd;
    // SAFETY: releasing a lock on this still-open descriptor is safe.
    unsafe {
        libc::flock(file.as_raw_fd(), libc::LOCK_UN);
    }
}

#[cfg(not(unix))]
fn lock_file(_: &File) -> Result<()> {
    Ok(())
}
#[cfg(not(unix))]
fn unlock_file(_: &File) {}

fn now_ms() -> i128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i128
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(at: &str, until: &str) -> Hold {
        Hold {
            id: String::new(),
            resource: "github:org/repo#42".into(),
            scope: "merge only".into(),
            owner: "agent-a".into(),
            event_at: at.into(),
            until: until.into(),
            reason: "waiting for CI".into(),
        }
    }

    #[test]
    fn ids_are_stable_and_hold_is_active_at_half_open_deadline() {
        let mut store = HoldStore {
            version: 1,
            events: vec![],
        };
        let at = floor_millis(OffsetDateTime::now_utc());
        let start = at.format(&Rfc3339).unwrap();
        let until = (at + time::Duration::seconds(30)).format(&Rfc3339).unwrap();
        let hold = set_hold(&mut store, input(&start, &until)).unwrap();
        assert_eq!(hold.id.len(), 64);
        assert!(
            active(
                &hold,
                OffsetDateTime::parse(&hold.event_at, &Rfc3339)
                    .unwrap()
                    .unix_timestamp_nanos()
                    .div_euclid(1_000_000)
            )
            .unwrap()
        );
        assert!(
            !active(
                &hold,
                OffsetDateTime::parse(&hold.until, &Rfc3339)
                    .unwrap()
                    .unix_timestamp_nanos()
                    .div_euclid(1_000_000)
            )
            .unwrap()
        );
    }

    #[test]
    fn hold_reference_requires_current_active_id() {
        let mut store = HoldStore {
            version: 1,
            events: vec![],
        };
        let at = floor_millis(OffsetDateTime::now_utc());
        let start = at.format(&Rfc3339).unwrap();
        let end = (at + time::Duration::seconds(30)).format(&Rfc3339).unwrap();
        let hold = set_hold(&mut store, input(&start, &end)).unwrap();
        assert!(check_id(&store, &hold.id).unwrap().active);
        assert!(prepare_hold_message(Path::new("/missing"), &hold.id, "hello").is_err());
    }

    #[test]
    fn set_rejects_renewing_a_live_hold_and_release_requires_owner() {
        let mut store = HoldStore {
            version: 1,
            events: vec![],
        };
        let at = floor_millis(OffsetDateTime::now_utc());
        let start = at.format(&Rfc3339).unwrap();
        let until = (at + time::Duration::seconds(20)).format(&Rfc3339).unwrap();
        set_hold(&mut store, input(&start, &until)).unwrap();
        assert!(set_hold(&mut store, input(&start, &until)).is_ok());
        let mut other = Release {
            resource: "github:org/repo#42".into(),
            owner: "other".into(),
            event_at: start,
            reason: "done".into(),
        };
        assert!(release_hold(&mut store, other.clone()).is_err());
        other.owner = "agent-a".into();
        assert!(release_hold(&mut store, other).is_ok());
    }

    #[test]
    fn hold_store_roundtrips_the_legacy_event_envelope() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("communication-holds.json");
        let at = floor_millis(OffsetDateTime::now_utc());
        let start = at.format(&Rfc3339).unwrap();
        let until = (at + time::Duration::seconds(30)).format(&Rfc3339).unwrap();
        let hold = with_store_lock(&path, |store| set_hold(store, input(&start, &until))).unwrap();
        let raw: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(raw["version"], 1);
        assert_eq!(raw["events"][0]["kind"], "hold");
        assert_eq!(raw["events"][0]["hold"]["id"], hold.id);
        assert!(
            check_id(&load_store(&path).unwrap(), &hold.id)
                .unwrap()
                .active
        );
    }

    fn floor_millis(value: OffsetDateTime) -> OffsetDateTime {
        let nanos = value.unix_timestamp_nanos().div_euclid(1_000_000) * 1_000_000;
        OffsetDateTime::from_unix_timestamp_nanos(nanos).unwrap()
    }
}
