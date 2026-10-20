//! Optional Linux workload budgets. Values remain separate from OS adapters.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ResourcesConfig {
    pub linux: LinuxResources,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LinuxResources {
    pub enabled: bool,
    /// Equal group names share one budget for this Unix user on this host.
    pub group: String,
    pub shared: ResourceLimits,
    pub job: ResourceLimits,
}

impl Default for LinuxResources {
    fn default() -> Self {
        Self {
            enabled: true,
            group: String::new(),
            shared: ResourceLimits::default(),
            job: ResourceLimits::default(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ResourceLimits {
    /// 100 means one CPU's worth of time, shared by all member processes.
    pub cpu_quota_percent: Option<u32>,
    pub memory_high: Option<String>,
    pub memory_max: Option<String>,
    pub memory_swap_max: Option<String>,
}

impl ResourceLimits {
    pub fn is_empty(&self) -> bool {
        self.cpu_quota_percent.is_none()
            && self.memory_high.is_none()
            && self.memory_max.is_none()
            && self.memory_swap_max.is_none()
    }

    fn validate(&self, path: &str, errors: &mut Vec<String>) {
        if self.cpu_quota_percent == Some(0) {
            errors.push(format!("{path}.cpu_quota_percent must be positive"));
        }
        for (name, value) in [
            ("memory_high", &self.memory_high),
            ("memory_max", &self.memory_max),
            ("memory_swap_max", &self.memory_swap_max),
        ] {
            if let Some(value) = value {
                match memory_bytes(value) {
                    Some(0) if name != "memory_swap_max" => {
                        errors.push(format!("{path}.{name} must be positive"));
                    }
                    None => errors.push(format!(
                        "{path}.{name}: expected bytes or a whole number with K, M, G, or T suffix"
                    )),
                    _ => {}
                }
            }
        }
        let high = self.memory_high.as_deref().and_then(memory_bytes);
        let max = self.memory_max.as_deref().and_then(memory_bytes);
        if let (Some(high), Some(max)) = (high, max)
            && high > max
        {
            errors.push(format!("{path}.memory_high cannot exceed memory_max"));
        }
    }
}

impl LinuxResources {
    pub fn active(&self) -> bool {
        self.enabled && (!self.shared.is_empty() || !self.job.is_empty())
    }

    pub(crate) fn validate(&self, errors: &mut Vec<String>) {
        if self.active()
            && (self.group.is_empty()
                || self.group.len() > 64
                || !self
                    .group
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_'))
        {
            errors.push(
                "resources.linux.group: use 1-64 ASCII letters, digits or underscores".into(),
            );
        }
        self.shared.validate("resources.linux.shared", errors);
        self.job.validate("resources.linux.job", errors);
    }
}

/// Normalize a bounded configuration value before passing it to an OS helper.
pub fn memory_bytes(value: &str) -> Option<u64> {
    let (digits, power) = match value.as_bytes().last().copied()? {
        b'K' => (&value[..value.len() - 1], 1),
        b'M' => (&value[..value.len() - 1], 2),
        b'G' => (&value[..value.len() - 1], 3),
        b'T' => (&value[..value.len() - 1], 4),
        _ => (value, 0),
    };
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse::<u64>().ok()?.checked_mul(1024_u64.pow(power))
}
