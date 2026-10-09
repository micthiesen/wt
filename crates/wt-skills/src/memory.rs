use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
};
use thiserror::Error;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillsMemory {
    #[serde(default)]
    pub answers: BTreeMap<String, String>,
    #[serde(default)]
    pub declined: BTreeMap<String, String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Error)]
pub enum MemoryError {
    #[error("read skills memory: {0}")]
    Read(#[from] std::io::Error),
    #[error("parse skills memory: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("skills memory is locked by another process")]
    Locked,
}

#[derive(Clone, Debug)]
pub struct MemoryStore {
    path: PathBuf,
}

impl MemoryStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn load(&self) -> Result<SkillsMemory, MemoryError> {
        match fs::read(&self.path) {
            Ok(raw) => Ok(parse_memory(serde_json::from_slice(&raw)?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(SkillsMemory::default())
            }
            Err(error) => Err(error.into()),
        }
    }
    pub fn save(&self, memory: &SkillsMemory) -> Result<(), MemoryError> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| std::io::Error::other("memory path has no parent"))?;
        fs::create_dir_all(parent)?;
        let _lock = MemoryLock::acquire(&self.path)?;
        self.save_locked(memory, parent)
    }
    fn save_locked(&self, memory: &SkillsMemory, parent: &Path) -> Result<(), MemoryError> {
        let tmp = self
            .path
            .with_extension(format!("json.{}.tmp", std::process::id()));
        let mut file = fs::File::create(&tmp)?;
        serde_json::to_writer_pretty(&mut file, memory)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&tmp, &self.path)?;
        if let Ok(dir) = fs::File::open(parent) {
            let _ = dir.sync_all();
        }
        Ok(())
    }
    pub fn update(
        &self,
        edit: impl FnOnce(&mut SkillsMemory),
    ) -> Result<SkillsMemory, MemoryError> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| std::io::Error::other("memory path has no parent"))?;
        fs::create_dir_all(parent)?;
        let _lock = MemoryLock::acquire(&self.path)?;
        let mut value = self.load()?;
        edit(&mut value);
        self.save_locked(&value, parent)?;
        Ok(value)
    }
}

fn parse_memory(value: serde_json::Value) -> SkillsMemory {
    let Some(mut object) = value.as_object().cloned() else {
        return SkillsMemory::default();
    };
    let answers = object
        .remove("answers")
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(key, value)| value.as_str().map(|value| (key, value.to_owned())))
        .collect();
    let declined = object
        .remove("declined")
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(key, value)| value.as_str().map(|value| (key, value.to_owned())))
        .collect();
    SkillsMemory {
        answers,
        declined,
        extra: object.into_iter().collect(),
    }
}

struct MemoryLock(fs::File);
impl MemoryLock {
    fn acquire(path: &Path) -> Result<Self, MemoryError> {
        let lock_path = path.with_extension("json.lock");
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            // flock is released by the kernel even if wt crashes.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        Ok(Self(file))
    }
}
impl Drop for MemoryLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let _ = unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unknown_fields_survive_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("skills.json");
        fs::write(&path, r#"{"answers":{"a":"b"},"future":{"x":1}}"#).unwrap();
        let store = MemoryStore::new(&path);
        let mut mem = store.load().unwrap();
        mem.declined.insert("skill:x".into(), "hash".into());
        store.save(&mem).unwrap();
        let raw: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(raw["future"]["x"], 1);
        assert_eq!(raw["declined"]["skill:x"], "hash");
    }

    #[test]
    fn invalid_known_entries_do_not_discard_the_rest_of_memory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("skills.json");
        fs::write(
            &path,
            r#"{"answers":{"valid":"yes","bad":3},"declined":{"x":"hash"}}"#,
        )
        .unwrap();
        let memory = MemoryStore::new(path).load().unwrap();
        assert_eq!(memory.answers.get("valid").map(String::as_str), Some("yes"));
        assert!(!memory.answers.contains_key("bad"));
        assert_eq!(memory.declined.get("x").map(String::as_str), Some("hash"));
    }
}
