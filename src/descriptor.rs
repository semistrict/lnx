use std::{fs, path::PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::paths::Layout;

const DESCRIPTOR_FILE: &str = "lnx.json";

/// Per-instance descriptor persisted next to the instance image. Settings are
/// optional overrides: the effective value is explicit CLI flag, then the
/// descriptor, then the built-in default.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct InstanceDescriptor {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<String>,
    /// Where the rootfs came from, e.g. "release:images-v0.2.0" or an OCI
    /// reference once image import exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpus: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mib: Option<u32>,
}

pub fn path(layout: &Layout) -> PathBuf {
    layout
        .base
        .join("instances")
        .join(&layout.instance)
        .join(DESCRIPTOR_FILE)
}

pub fn load(layout: &Layout) -> Result<InstanceDescriptor> {
    let path = path(layout);
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(InstanceDescriptor::default());
        }
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    serde_json::from_str(&contents).with_context(|| format!("parse {}", path.display()))
}

pub fn save(layout: &Layout, descriptor: &InstanceDescriptor) -> Result<()> {
    let path = path(layout);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(descriptor).context("encode descriptor")?;
    fs::write(&path, json + "\n").with_context(|| format!("write {}", path.display()))
}

/// Record identity fields the first time an instance image materializes;
/// settings already present are preserved.
pub fn ensure_identity(layout: &Layout, image: &str) -> Result<()> {
    let mut descriptor = load(layout)?;
    let mut dirty = false;
    if descriptor.name.is_none() {
        descriptor.name = Some(layout.instance.clone());
        dirty = true;
    }
    if descriptor.created.is_none() {
        descriptor.created = OffsetDateTime::now_utc().format(&Rfc3339).ok();
        dirty = true;
    }
    if descriptor.image.is_none() {
        descriptor.image = Some(image.to_string());
        dirty = true;
    }
    if dirty {
        save(layout, &descriptor)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
