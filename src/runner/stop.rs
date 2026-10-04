//! Stopping a running instance.

use super::*;

/// Stops the instance's VM owner the careful way: asks it to take its final
/// snapshot and exit, and waits for that, however long it takes up to
/// `timeout`. It never kills an owner, so a slow snapshot is not lost; an
/// owner still running at the deadline is left running and reported. A VM
/// that had died after serving commands is reported as needing `recover`.
pub(crate) fn stop_owner(layout: &Layout, timeout: Duration) -> Result<()> {
    if !layout.instance_dir.exists() && !layout.run_dir.exists() {
        return Ok(());
    }
    let deadline = Instant::now() + timeout;
    let current_owner = || live_owner(layout).map(|lease| lease.process);
    let mut signaled = current_owner();
    if let Some(owner) = signaled {
        // The process that launched the initial command keeps owner-start.lock
        // until that command returns. Signal the established owner first so
        // shutdown can release that client and, in turn, the start lock.
        owner.signal(libc::SIGTERM)?;
    }
    let _start_lock = loop {
        if let Some(lock) = OwnerStartLock::try_acquire(layout)? {
            break lock;
        }
        if let Some(owner) = current_owner()
            && signaled != Some(owner)
        {
            owner.signal(libc::SIGTERM)?;
            signaled = Some(owner);
        }
        if Instant::now() >= deadline {
            bail!(
                "instance {} did not finish starting within {} seconds; retry stop",
                layout.instance,
                timeout.as_secs_f64()
            );
        }
        thread::sleep(Duration::from_millis(10));
    };
    // With the start lock held no new owner can appear, so wait for whoever
    // holds the instance lock (an owner or a state copy) to let go.
    let owner = loop {
        let state = instance_lock_state(layout)?;
        match &state {
            InstanceLockState::Free { .. } => return ensure_shutdown_state_under_guard(layout),
            InstanceLockState::Held { lease } => {
                if let Some(lease) = lease
                    && lease.role == LeaseRole::Owner
                {
                    if signaled != Some(lease.process) {
                        lease.process.signal(libc::SIGTERM)?;
                    }
                    break lease.process;
                }
            }
        }
        if Instant::now() >= deadline {
            bail!(
                "instance {} state operation did not finish within {} seconds; retry stop",
                layout.instance,
                timeout.as_secs_f64()
            );
        }
        thread::sleep(Duration::from_millis(10));
    };
    loop {
        match current_owner() {
            None => return ensure_shutdown_state_under_guard(layout),
            Some(current) if current != owner => bail!(
                "instance {} changed VM owner from pid {} to pid {} while stopping",
                layout.instance,
                owner.pid,
                current.pid
            ),
            Some(_) => {}
        }
        if Instant::now() >= deadline {
            bail!(
                "owner process {} for instance {} did not finish its shutdown snapshot within {} seconds; it was left running so recoverable state is not discarded",
                owner.pid,
                layout.instance,
                timeout.as_secs_f64()
            );
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// Checks, under the instance guard, that the stopped owner left the
/// instance usable rather than crashed with unsaved state.
fn ensure_shutdown_state_under_guard(layout: &Layout) -> Result<()> {
    let checked = with_instance_guard(layout, |state| {
        if state.is_held() {
            return Ok(false);
        }
        refuse_crashed_run_unguarded(layout)?;
        Ok(true)
    })?;
    if !checked {
        bail!(
            "instance {} acquired a new VM owner while shutdown was being verified; retry stop",
            layout.instance
        );
    }
    Ok(())
}
