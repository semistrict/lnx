// Copyright 2026 The libkrun Authors.
// SPDX-License-Identifier: Apache-2.0

use std::fs::OpenOptions;
use std::path::Path;
use std::sync::Arc;

use vm_memory::mmap::{GuestRegionMmap, MmapRegion};
use vm_memory::{FileOffset, GuestAddress};
use vmm_sys_util::align_upwards;

#[derive(Clone, Debug)]
pub struct PmemDeviceConfig {
    pub id: String,
    pub path: String,
    pub read_only: bool,
}

#[derive(Clone, Debug)]
pub struct PmemRegionConfig {
    pub id: String,
    pub path: String,
    pub guest_addr: u64,
    pub size: u64,
}

#[derive(Default)]
pub struct PmemBuilder {
    devices: Vec<PmemDeviceConfig>,
}

impl PmemBuilder {
    pub fn insert(&mut self, config: PmemDeviceConfig) {
        self.devices.push(config);
    }

    pub fn list(&self) -> &[PmemDeviceConfig] {
        &self.devices
    }
}

pub fn build_pmem_regions(
    configs: &[PmemDeviceConfig],
    mut guest_addr: u64,
    page_size: usize,
) -> Result<(Vec<GuestRegionMmap>, Vec<PmemRegionConfig>), String> {
    let mut mappings = Vec::new();
    let mut regions = Vec::new();

    for config in configs {
        let file = OpenOptions::new()
            .read(true)
            .write(!config.read_only)
            .open(Path::new(&config.path))
            .map_err(|e| format!("open pmem {}: {e}", config.path))?;
        let size = file
            .metadata()
            .map_err(|e| format!("stat pmem {}: {e}", config.path))?
            .len();
        if size == 0 {
            return Err(format!("pmem {} is empty", config.path));
        }
        if size % page_size as u64 != 0 {
            return Err(format!(
                "pmem {} size {} is not page aligned",
                config.path, size
            ));
        }

        guest_addr = align_upwards!(guest_addr as usize, page_size) as u64;
        let mapping = MmapRegion::from_file(FileOffset::from_arc(Arc::new(file), 0), size as usize)
            .map_err(|e| format!("mmap pmem {}: {e:?}", config.path))?;
        let region = GuestRegionMmap::new(mapping, GuestAddress(guest_addr))
            .ok_or_else(|| format!("invalid pmem guest region {}", config.path))?;

        mappings.push(region);
        regions.push(PmemRegionConfig {
            id: config.id.clone(),
            path: config.path.clone(),
            guest_addr,
            size,
        });
        guest_addr += size;
    }

    Ok((mappings, regions))
}
