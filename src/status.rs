//! What an instance is doing, as seen by commands that do not own it.

use std::fmt;
use std::process::Command;

use serde::Serialize;

use crate::paths::{Layout, RuntimeSocket};
use crate::runner;

/// Ordered so that sorting lists active instances first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum InstanceState {
    /// A VM owner is accepting commands.
    Running,
    /// A VM owner holds the instance but is not accepting commands yet.
    Starting,
    Stopped,
    /// The instance directory exists but has no rootfs.
    Partial,
}

impl InstanceState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Starting => "starting",
            Self::Stopped => "stopped",
            Self::Partial => "partial",
        }
    }

    pub(crate) fn is_active(self) -> bool {
        matches!(self, Self::Running | Self::Starting)
    }
}

impl fmt::Display for InstanceState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

pub(crate) fn instance_state(layout: &Layout) -> InstanceState {
    let broker = layout.socket(RuntimeSocket::Broker);
    if broker.exists() && runner::connect_broker(&broker).is_ok() {
        InstanceState::Running
    } else if runner::live_owner(layout).is_some() {
        InstanceState::Starting
    } else if crate::init::instance_has_state(layout) {
        InstanceState::Stopped
    } else {
        InstanceState::Partial
    }
}

/// The VM owner plus any other lnx processes started for this instance.
pub(crate) fn instance_pids(layout: &Layout) -> Vec<i32> {
    let mut pids: Vec<i32> = runner::live_owner(layout)
        .map(|owner| owner.pid())
        .into_iter()
        .chain(host_pids_for_instance(&layout.instance))
        .collect();
    pids.sort_unstable();
    pids.dedup();
    pids
}

fn host_pids_for_instance(instance: &str) -> Vec<i32> {
    let Ok(output) = Command::new("pgrep")
        .arg("-f")
        .arg(format!("--instance[= ]{instance}"))
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let current = std::process::id() as i32;
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().parse::<i32>().ok())
        .filter(|&pid| pid != current && runner::ProcessIdentity::of(pid).is_some())
        .collect()
}
