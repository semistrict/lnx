# Testing

Rust unit tests:

```sh
bun run test
```

Integration suites (each `bun run test:<name>` builds first and runs one
suite; see `package.json` for the full list):

```sh
bun run test:system        # core exec, shares, forwarding
bun run test:checkpoint    # checkpoint/fork behavior
bun run test:snapshot-roundtrip
bun run test:full          # everything CI runs
```

Nested KVM coverage (Linux-host paths inside an outer lnx guest):

```sh
bun run test:nested-kvm
```

## Opt-in suites and prerequisites

- Browser pixel/cursor verification still needs a reliable VNC client
  dependency such as vncdotool or a Playwright-controlled noVNC canvas check.
  `scripts/test/browser-snapshot.ts` is opt-in with `LNX_RUN_BROWSER_TEST=1`
  and currently verifies stock snap Chromium install plus noVNC endpoint
  survival across checkpoint/fork, but not actual rendered pixels or cursor
  visibility.
- Privileged ingress launchd install is opt-in with
  `LNX_RUN_PRIVILEGED_INGRESS_TEST=1` because it uses sudo, `/etc/resolver`,
  launchd, and privileged ports.
- Dirty filesystem offline fsck requires host `e2fsck`; the test skips clearly
  when it is unavailable.

## Known coverage gaps

- `system` and `stress` have nested-safe coverage for their non-snapshot
  behavior; their snapshot-specific assertions should move into the nested
  Linux suite after the Linux full-RAM restore path has end-to-end runtime
  coverage.
- `stock-ubuntu` remains excluded from the nested suite: `snapd` panics while
  parsing the nested guest kernel command line under nested KVM, and a stock
  boot/apt probe hung instead of producing bounded signal.
- Browser snapshot coverage remains opt-in and snapshot/fork-dependent.
- Ingress and privileged ingress tests are macOS host tests because they
  depend on launchd, `/etc/resolver`, keychain/sudo setup, and privileged
  host ports.
