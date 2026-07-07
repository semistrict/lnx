use std::collections::BTreeMap;
use std::fs;
use std::io::{ErrorKind, Read};
use std::path::Path;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::paths::Layout;

use super::{
    DETERMINISTIC_CLOCK_STATE, DETERMINISTIC_TIMER_JUMPS, DETERMINISTIC_TIMER_JUMPS_CURSOR,
    DeterministicConfig, TraceLog, parse_shares_stamp, trace_integer,
};

pub(crate) const RESTORE_ENTROPY_BYTES: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeterministicClockState {
    pub(crate) realtime_unix_nanos: u64,
    pub(crate) monotonic_nanos: u64,
    pub(crate) counter_frequency_hz: u64,
    pub(crate) event_sequence: u64,
    pub(crate) timer_jump_count: u64,
    pub(crate) last_timer_deadline_ticks: u64,
}

pub fn validate_runtime_deterministic_compatibility(
    layout: &Layout,
    deterministic: Option<&DeterministicConfig>,
) -> Result<()> {
    let current = deterministic_stamp_content(deterministic);
    let stamp_path = layout.run_dir.join("deterministic.stamp");
    match fs::read_to_string(&stamp_path) {
        Ok(stamp) if stamp == current => Ok(()),
        Ok(stamp) => bail!(
            "running VM deterministic stamp is incompatible ({}): {}",
            describe_deterministic_stamp_mismatch(&stamp, &current),
            stamp_path.display()
        ),
        Err(e)
            if e.kind() == std::io::ErrorKind::NotFound
                && current == deterministic_stamp_content(None) =>
        {
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!(
            "running VM has no deterministic compatibility stamp: {}",
            stamp_path.display()
        ),
        Err(e) => Err(e).with_context(|| format!("read {}", stamp_path.display())),
    }
}

pub(crate) fn restore_entropy(config: Option<&DeterministicConfig>) -> Result<Vec<u8>> {
    if let Some(config) = config {
        return Ok(deterministic_restore_entropy(&config.seed));
    }
    fresh_restore_entropy()
}

pub(crate) fn deterministic_restore_entropy(seed: &str) -> Vec<u8> {
    let mut entropy = Vec::with_capacity(RESTORE_ENTROPY_BYTES);
    let mut counter = 0u64;
    while entropy.len() < RESTORE_ENTROPY_BYTES {
        let mut hasher = Sha256::new();
        hasher.update(b"lnx deterministic restore entropy v1\0");
        hasher.update(seed.as_bytes());
        hasher.update(b"\0");
        hasher.update(counter.to_le_bytes());
        entropy.extend_from_slice(&hasher.finalize());
        counter = counter.saturating_add(1);
    }
    entropy.truncate(RESTORE_ENTROPY_BYTES);
    entropy
}

fn fresh_restore_entropy() -> Result<Vec<u8>> {
    let mut entropy = vec![0u8; RESTORE_ENTROPY_BYTES];
    fs::File::open("/dev/urandom")
        .context("open host /dev/urandom for restore entropy")?
        .read_exact(&mut entropy)
        .context("read host restore entropy")?;
    Ok(entropy)
}

pub(crate) fn deterministic_stamp_content(config: Option<&DeterministicConfig>) -> String {
    match config {
        Some(config) => format!(
            "deterministic=enabled-v1\nseed={}\ninitial_realtime_unix_secs=0\nclock_state=deterministic-clock-state-v1\nrestore_timer_rebase=disabled-v1\nvirtual_counter=kvm-controlled-counter-v1\nkvm_halt_poll=disabled-v1\nkvm_wfi_exit=enabled-v1\nhost_activity_gate=broker-and-device-idle-v1\nrtc=deterministic-zero-v1\ntrng=deterministic-smccc-v1\nvirtio_rng=deterministic-stateless-v1\nvsock_timesync=disabled-v1\nrestore_entropy=sha256-seed-v1\nexec_user=uid1000-gid1000-lnxuser\nexec_env=c-utf8-utc-v1\nexec_tty=none-24x80-xterm-256color-v1\nnetwork=gvproxy-fixed-v1\n",
            config.seed
        ),
        None => "deterministic=disabled-v1\n".to_string(),
    }
}

pub(crate) fn configure_libkrun_deterministic_time(enabled: bool) {
    unsafe {
        if enabled {
            std::env::set_var("KRUN_DETERMINISTIC_TIME", "1");
        } else {
            std::env::remove_var("KRUN_DETERMINISTIC_TIME");
        }
    }
}

pub(crate) fn configure_libkrun_deterministic_clock_state(path: Option<&Path>) {
    unsafe {
        if let Some(path) = path {
            std::env::set_var("KRUN_DETERMINISTIC_CLOCK_STATE", path);
        } else {
            std::env::remove_var("KRUN_DETERMINISTIC_CLOCK_STATE");
        }
    }
}

pub(crate) fn configure_libkrun_deterministic_timer_jumps(path: Option<&Path>) {
    unsafe {
        if let Some(path) = path {
            std::env::set_var("KRUN_DETERMINISTIC_TIMER_JUMPS", path);
        } else {
            std::env::remove_var("KRUN_DETERMINISTIC_TIMER_JUMPS");
        }
    }
}

pub(crate) fn initial_deterministic_clock_state() -> DeterministicClockState {
    DeterministicClockState {
        realtime_unix_nanos: 0,
        monotonic_nanos: 0,
        counter_frequency_hz: 1_000_000_000,
        event_sequence: 0,
        timer_jump_count: 0,
        last_timer_deadline_ticks: 0,
    }
}

fn deterministic_clock_state_content(state: &DeterministicClockState) -> String {
    format!(
        "clock_state=deterministic-clock-state-v1\nrealtime_unix_nanos={}\nmonotonic_nanos={}\ncounter_frequency_hz={}\nevent_sequence={}\ntimer_jump_count={}\nlast_timer_deadline_ticks={}\n",
        state.realtime_unix_nanos,
        state.monotonic_nanos,
        state.counter_frequency_hz,
        state.event_sequence,
        state.timer_jump_count,
        state.last_timer_deadline_ticks
    )
}

pub(crate) fn parse_deterministic_clock_state(raw: &str) -> Result<DeterministicClockState> {
    let fields = parse_shares_stamp(raw);
    match fields.get("clock_state").map(String::as_str) {
        Some("deterministic-clock-state-v1") => {}
        Some(other) => bail!("unsupported deterministic clock state {other}"),
        None => bail!("missing deterministic clock state version"),
    }
    Ok(DeterministicClockState {
        realtime_unix_nanos: parse_clock_state_u64(&fields, "realtime_unix_nanos")?,
        monotonic_nanos: parse_clock_state_u64(&fields, "monotonic_nanos")?,
        counter_frequency_hz: parse_clock_state_u64(&fields, "counter_frequency_hz")?,
        event_sequence: parse_clock_state_u64(&fields, "event_sequence")?,
        timer_jump_count: parse_clock_state_u64(&fields, "timer_jump_count")?,
        last_timer_deadline_ticks: parse_clock_state_u64(&fields, "last_timer_deadline_ticks")?,
    })
}

fn parse_clock_state_u64(fields: &BTreeMap<String, String>, key: &str) -> Result<u64> {
    let value = fields
        .get(key)
        .with_context(|| format!("missing deterministic clock state field {key}"))?;
    value
        .parse()
        .with_context(|| format!("parse deterministic clock state field {key}={value}"))
}

pub(crate) fn deterministic_clock_state_for_start(
    deterministic: Option<&DeterministicConfig>,
    restore_snapshot: Option<&Path>,
) -> Result<Option<DeterministicClockState>> {
    if deterministic.is_none() {
        return Ok(None);
    }
    let Some(snapshot) = restore_snapshot else {
        return Ok(Some(initial_deterministic_clock_state()));
    };
    read_deterministic_clock_state(snapshot)
        .with_context(|| {
            format!(
                "read {}",
                snapshot.join(DETERMINISTIC_CLOCK_STATE).display()
            )
        })
        .map(Some)
}

pub(crate) fn read_deterministic_clock_state(snapshot: &Path) -> Result<DeterministicClockState> {
    let path = snapshot.join(DETERMINISTIC_CLOCK_STATE);
    let raw = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    parse_deterministic_clock_state(&raw)
}

pub(crate) fn write_deterministic_clock_state(
    path: &Path,
    state: &DeterministicClockState,
) -> Result<()> {
    fs::write(path, deterministic_clock_state_content(state))
        .with_context(|| format!("write {}", path.display()))
}

pub(crate) fn ensure_deterministic_clock_state_file(
    initramfs_stamp: &Path,
    state: Option<&DeterministicClockState>,
) -> Result<()> {
    let Some(state) = state else {
        return Ok(());
    };
    let path = initramfs_stamp.with_file_name(DETERMINISTIC_CLOCK_STATE);
    if path.exists() {
        return Ok(());
    }
    write_deterministic_clock_state(&path, state)
}

pub(crate) fn flush_deterministic_trace_events(
    layout: &Layout,
    trace_log: Option<&TraceLog>,
) -> Result<()> {
    let initramfs_stamp = layout.run_dir.join("initramfs.stamp");
    import_deterministic_timer_jumps(&initramfs_stamp, trace_log)?;
    sync_deterministic_clock_event_sequence(&initramfs_stamp, trace_log)
}

pub(crate) fn sync_deterministic_clock_event_sequence(
    initramfs_stamp: &Path,
    trace_log: Option<&TraceLog>,
) -> Result<()> {
    let Some(trace_log) = trace_log else {
        return Ok(());
    };
    let path = initramfs_stamp.with_file_name(DETERMINISTIC_CLOCK_STATE);
    if !path.exists() {
        return Ok(());
    }
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    let mut state = parse_deterministic_clock_state(&raw)?;
    state.event_sequence = trace_log.next_sequence();
    write_deterministic_clock_state(&path, &state)
}

pub(crate) fn import_deterministic_timer_jumps(
    initramfs_stamp: &Path,
    trace_log: Option<&TraceLog>,
) -> Result<()> {
    let Some(trace_log) = trace_log else {
        return Ok(());
    };
    let jumps_path = initramfs_stamp.with_file_name(DETERMINISTIC_TIMER_JUMPS);
    if !jumps_path.exists() {
        return Ok(());
    }
    let cursor_path = initramfs_stamp.with_file_name(DETERMINISTIC_TIMER_JUMPS_CURSOR);
    let cursor = fs::read_to_string(&cursor_path)
        .ok()
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let raw = fs::read_to_string(&jumps_path)
        .with_context(|| format!("read {}", jumps_path.display()))?;
    let start = cursor.min(raw.len());
    let mut consumed = start;
    for line in raw[start..].lines() {
        consumed = consumed.saturating_add(line.len() + 1);
        let fields = parse_timer_jump_line(line);
        let Some(deadline_ticks) = fields.get("deadline_ticks").copied() else {
            continue;
        };
        let Some(counter_frequency_hz) = fields.get("counter_frequency_hz").copied() else {
            continue;
        };
        let Some(deadline_nanos) = fields.get("deadline_nanos").copied() else {
            continue;
        };
        trace_log.event(
            "timer_jump",
            vec![
                trace_integer("deadline_ticks", deadline_ticks as i64),
                trace_integer("counter_frequency_hz", counter_frequency_hz as i64),
                trace_integer("deadline_nanos", deadline_nanos as i64),
            ],
        );
    }
    fs::write(&cursor_path, consumed.to_string())
        .with_context(|| format!("write {}", cursor_path.display()))
}

fn parse_timer_jump_line(line: &str) -> BTreeMap<String, u64> {
    line.split_whitespace()
        .filter_map(|part| {
            let (key, value) = part.split_once('=')?;
            Some((key.to_string(), value.parse().ok()?))
        })
        .collect()
}

pub(crate) fn snapshot_deterministic_incompatibility(
    snapshot_path: &Path,
    current: &str,
) -> Option<String> {
    match fs::read_to_string(snapshot_path.join("deterministic.stamp")) {
        Ok(stamp) if stamp == current => None,
        Ok(stamp) => Some(describe_deterministic_stamp_mismatch(&stamp, current)),
        Err(e)
            if e.kind() == std::io::ErrorKind::NotFound
                && current == deterministic_stamp_content(None) =>
        {
            None
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Some("snapshot has no deterministic compatibility stamp".to_string())
        }
        Err(e) => Some(format!("deterministic_stamp_unreadable: {e}")),
    }
}

fn describe_deterministic_stamp_mismatch(snapshot: &str, current: &str) -> String {
    let snapshot_fields = parse_shares_stamp(snapshot);
    let current_fields = parse_shares_stamp(current);
    let mut mismatches = Vec::new();
    for key in [
        "deterministic",
        "seed",
        "initial_realtime_unix_secs",
        "clock_state",
        "restore_timer_rebase",
        "virtual_counter",
        "kvm_halt_poll",
        "kvm_wfi_exit",
        "host_activity_gate",
        "rtc",
        "trng",
        "virtio_rng",
        "vsock_timesync",
        "restore_entropy",
        "exec_user",
        "exec_env",
        "exec_tty",
        "network",
    ] {
        let snapshot_value = snapshot_fields
            .get(key)
            .map(String::as_str)
            .unwrap_or("<absent>");
        let current_value = current_fields
            .get(key)
            .map(String::as_str)
            .unwrap_or("<absent>");
        if snapshot_value != current_value {
            mismatches.push(format!(
                "{key}: snapshot={snapshot_value} current={current_value}"
            ));
        }
    }
    if mismatches.is_empty() {
        "snapshot and current deterministic stamps differ only in unrecognized fields".to_string()
    } else {
        mismatches.join("; ")
    }
}

pub(crate) fn deterministic_exec_request_id(
    seed: &str,
    command: &[String],
    guest_cwd: &str,
    run_as_root: bool,
    pty: bool,
    rows: u16,
    cols: u16,
) -> u64 {
    let mut hasher = Sha256::new();
    hasher.update(b"lnx deterministic exec request id v1\0");
    hasher.update(seed.as_bytes());
    hasher.update(b"\0cwd\0");
    hasher.update(guest_cwd.as_bytes());
    hasher.update(b"\0root\0");
    hasher.update([u8::from(run_as_root)]);
    hasher.update(b"\0pty\0");
    hasher.update([u8::from(pty)]);
    hasher.update(b"\0rows\0");
    hasher.update(rows.to_le_bytes());
    hasher.update(b"\0cols\0");
    hasher.update(cols.to_le_bytes());
    for arg in command {
        hasher.update(b"\0arg\0");
        hasher.update(arg.as_bytes());
    }
    let digest = hasher.finalize();
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    let id = u64::from_le_bytes(bytes);
    if id == 0 { 1 } else { id }
}

pub(crate) fn deterministic_restore_sync_request_id(seed: &str) -> u64 {
    let mut hasher = Sha256::new();
    hasher.update(b"lnx deterministic restore-sync request id v1\0");
    hasher.update(seed.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    let id = u64::from_le_bytes(bytes);
    if id == 0 { 1 } else { id }
}
