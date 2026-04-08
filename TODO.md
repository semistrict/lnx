# Linux Host (Firecracker) — TODO

Experimental Linux/KVM backend (`LNX_EXPERIMENTS=linux_host`). Basic non-interactive exec works. The following needs testing/implementation:

## Not tested

- [ ] Interactive PTY (`lnx bash` inside nested VM)
- [ ] CWD mounting via 9P — nested `lnx ls .` won't see host files
- [ ] Extra share mounting via 9P
- [ ] Home dir 9P mount inside nested VM
- [ ] Port forwarding from nested guest to host
- [ ] SSH agent forwarding
- [ ] `lnx status`, `lnx stop`, `lnx sessions` for nested instances
- [ ] Internet connectivity from inside nested VM (TAP + NAT configured but unverified)
- [ ] Checkpoint / ephemeral mode
- [ ] Multiple concurrent exec sessions
- [ ] `lnx instance create/delete` for nested instances

## Known issues

- [ ] Nested client logging writes to read-only 9P mount (silently fails, not harmful)
- [ ] TAP device cleanup is best-effort — if Firecracker crashes, stale TAP may block next boot until outer VM restarts
- [ ] Nested VM requires `sudo` for daemon (handled automatically, but NOPASSWD sudoers required)
