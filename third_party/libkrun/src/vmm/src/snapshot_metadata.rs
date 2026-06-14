use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt::Write as _;

pub(crate) const VMSTATE_VERSION: u32 = 4;
pub(crate) const TOPOLOGY_HASH_VERSION: u32 = 1;
pub(crate) const GUEST_ARCH_AARCH64: &str = "aarch64";
pub(crate) const PAUTH_POLICY_NOPAUTH: &str = "arm64.nopauth";
pub(crate) const SOURCE_BACKEND_HVF: &str = "hvf";
pub(crate) const SOURCE_BACKEND_KVM: &str = "kvm";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SnapshotFormat {
    Macos,
    Linux,
}

impl SnapshotFormat {
    pub(crate) fn from_source_backend(source_backend: &str) -> Option<Self> {
        match source_backend {
            SOURCE_BACKEND_HVF => Some(Self::Macos),
            SOURCE_BACKEND_KVM => Some(Self::Linux),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RamRegion {
    pub guest_addr: u64,
    pub size: u64,
    pub file_offset: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RamLayout {
    pub regions: Vec<RamRegion>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MetaSection {
    pub ram: RamLayout,
    pub virtio_bases: Vec<u64>,
    pub vcpu_count: u32,
    pub nested_enabled: bool,
    pub source_backend: String,
    pub capture_timer_counter: u64,
    pub topology_hash_version: u32,
    pub topology_hash: [u8; 32],
    pub gic_topology: Option<GicTopology>,
    pub virtio_topology: Vec<VirtioTopology>,
    pub guest_arch: String,
    pub pauth_policy: String,
}

impl MetaSection {
    pub(crate) fn source_format(&self) -> Option<SnapshotFormat> {
        SnapshotFormat::from_source_backend(&self.source_backend)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GicTopology {
    pub compatibility: String,
    pub version: u32,
    pub maint_irq: u32,
    pub vcpu_count: u64,
    pub properties: Vec<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VirtioTopology {
    pub mmio_base: u64,
    pub device_name: String,
}

pub(crate) fn ram_ranges_from_layout(layout: &RamLayout) -> Vec<(u64, u64)> {
    layout
        .regions
        .iter()
        .map(|region| (region.guest_addr, region.size))
        .collect()
}

pub(crate) fn hash_hex(hash: &[u8; 32]) -> String {
    let mut out = String::with_capacity(64);
    for byte in hash {
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

pub(crate) fn topology_summary(
    ram_ranges: &[(u64, u64)],
    vcpu_count: u32,
    nested_enabled: bool,
    gic: Option<&GicTopology>,
    virtio_devices: &[VirtioTopology],
) -> String {
    let mut ranges = ram_ranges.to_vec();
    ranges.sort_unstable();
    let mut virtio = virtio_devices.to_vec();
    virtio.sort_by(|a, b| {
        a.mmio_base
            .cmp(&b.mmio_base)
            .then_with(|| a.device_name.cmp(&b.device_name))
    });

    format!(
        "ram={ranges:?} vcpus={vcpu_count} nested={nested_enabled} gic={gic:?} virtio={virtio:?}"
    )
}

pub(crate) fn compute_topology_hash(
    ram_ranges: &[(u64, u64)],
    vcpu_count: u32,
    nested_enabled: bool,
    gic: Option<&GicTopology>,
    virtio_devices: &[VirtioTopology],
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hash_str(&mut hasher, "lnx.snapshot.topology");
    hash_u32(&mut hasher, TOPOLOGY_HASH_VERSION);
    hash_str(&mut hasher, GUEST_ARCH_AARCH64);
    hash_u32(&mut hasher, vcpu_count);
    hash_bool(&mut hasher, nested_enabled);

    let mut ranges = ram_ranges.to_vec();
    ranges.sort_unstable();
    hash_u64(&mut hasher, ranges.len() as u64);
    for (base, size) in ranges {
        hash_u64(&mut hasher, base);
        hash_u64(&mut hasher, size);
    }

    hash_bool(&mut hasher, gic.is_some());
    if let Some(gic) = gic {
        // Raw GIC MMIO properties differ between HVF and KVM even when the
        // guest-visible virtual GIC model is compatible and translated.
        hash_str(&mut hasher, &gic.compatibility);
        hash_u32(&mut hasher, gic.version);
        hash_u64(&mut hasher, gic.vcpu_count);
    }

    let mut virtio = virtio_devices.to_vec();
    virtio.sort_by(|a, b| {
        a.mmio_base
            .cmp(&b.mmio_base)
            .then_with(|| a.device_name.cmp(&b.device_name))
    });
    hash_u64(&mut hasher, virtio.len() as u64);
    for device in virtio {
        hash_u64(&mut hasher, device.mmio_base);
        hash_str(&mut hasher, &device.device_name);
    }

    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

fn hash_str(hasher: &mut Sha256, value: &str) {
    hash_u64(hasher, value.len() as u64);
    hasher.update(value.as_bytes());
}

fn hash_u32(hasher: &mut Sha256, value: u32) {
    hasher.update(value.to_le_bytes());
}

fn hash_u64(hasher: &mut Sha256, value: u64) {
    hasher.update(value.to_le_bytes());
}

fn hash_bool(hasher: &mut Sha256, value: bool) {
    hasher.update([u8::from(value)]);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_gic() -> GicTopology {
        GicTopology {
            compatibility: "arm,gic-v3".to_string(),
            version: 3,
            maint_irq: 25,
            vcpu_count: 2,
            properties: vec![0x1000, 0x10000, 0x2000, 0x20000],
        }
    }

    #[test]
    fn topology_hash_is_order_stable() {
        let left = compute_topology_hash(
            &[(0x8000_0000, 0x1000), (0x4000_0000, 0x2000)],
            2,
            false,
            Some(&sample_gic()),
            &[
                VirtioTopology {
                    mmio_base: 0x2000,
                    device_name: "block".to_string(),
                },
                VirtioTopology {
                    mmio_base: 0x1000,
                    device_name: "net".to_string(),
                },
            ],
        );
        let right = compute_topology_hash(
            &[(0x4000_0000, 0x2000), (0x8000_0000, 0x1000)],
            2,
            false,
            Some(&sample_gic()),
            &[
                VirtioTopology {
                    mmio_base: 0x1000,
                    device_name: "net".to_string(),
                },
                VirtioTopology {
                    mmio_base: 0x2000,
                    device_name: "block".to_string(),
                },
            ],
        );

        assert_eq!(left, right);
    }

    #[test]
    fn topology_hash_changes_with_layout() {
        let baseline = compute_topology_hash(
            &[(0x4000_0000, 0x2000)],
            2,
            false,
            Some(&sample_gic()),
            &[VirtioTopology {
                mmio_base: 0x1000,
                device_name: "block".to_string(),
            }],
        );
        let changed = compute_topology_hash(
            &[(0x4000_0000, 0x3000)],
            2,
            false,
            Some(&sample_gic()),
            &[VirtioTopology {
                mmio_base: 0x1000,
                device_name: "block".to_string(),
            }],
        );

        assert_ne!(baseline, changed);
    }

    #[test]
    fn topology_hash_changes_with_backend_devices() {
        let baseline = compute_topology_hash(
            &[(0x4000_0000, 0x2000)],
            2,
            false,
            Some(&sample_gic()),
            &[VirtioTopology {
                mmio_base: 0x1000,
                device_name: "block".to_string(),
            }],
        );
        let changed = compute_topology_hash(
            &[(0x4000_0000, 0x2000)],
            2,
            false,
            Some(&sample_gic()),
            &[VirtioTopology {
                mmio_base: 0x1000,
                device_name: "net".to_string(),
            }],
        );

        assert_ne!(baseline, changed);
    }

    #[test]
    fn topology_hash_ignores_backend_specific_gic_layout() {
        let mut changed_gic = sample_gic();
        changed_gic.maint_irq = 31;
        changed_gic.properties = vec![0x3000, 0x30000, 0x4000, 0x40000];

        let baseline = compute_topology_hash(
            &[(0x4000_0000, 0x2000)],
            2,
            false,
            Some(&sample_gic()),
            &[VirtioTopology {
                mmio_base: 0x1000,
                device_name: "block".to_string(),
            }],
        );
        let changed = compute_topology_hash(
            &[(0x4000_0000, 0x2000)],
            2,
            false,
            Some(&changed_gic),
            &[VirtioTopology {
                mmio_base: 0x1000,
                device_name: "block".to_string(),
            }],
        );

        assert_eq!(baseline, changed);
    }
}
