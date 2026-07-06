# Security notes

`lnx` itself runs unprivileged: VMs use Apple's Hypervisor.framework via
libkrun, state lives in `~/.lnx`, and no daemon runs as root. The one feature
that touches system state is ingress, and it only does so when you explicitly
run `sudo lnx ingress enable`.

## What `lnx ingress enable` installs

Ingress gives every instance stable `https://p<port>-<instance>.lnx` URLs that
terminate TLS on the host and proxy to guest ports. Enabling it installs:

| What | Where | Why |
|---|---|---|
| Resolver file | `/etc/resolver/lnx` | Routes `.lnx` DNS lookups to a local resolver on 127.0.0.1 |
| launchd service | `/Library/LaunchDaemons` (or per-user LaunchAgents for unprivileged ports) | Runs the DNS/HTTP/HTTPS listeners |
| Local CA | `~/.lnx/ingress/ca`, trusted in the System keychain | Signs per-host `.lnx` certificates |

Nothing is sent off the machine. All listeners bind loopback addresses.

## The local CA is name-constrained

The generated CA carries an X.509 `nameConstraints` extension (critical)
permitting only `DNS:.lnx` names and excluding all IP addresses, plus
`basicConstraints` with `pathlen:0`. Even with the CA trusted in the System
keychain, certificates it signs are only valid for `.lnx` hosts — it cannot be
used to intercept traffic to any real domain. The CA key never leaves
`~/.lnx/ingress/ca` and each `ingress enable` regenerates it.

You can verify the constraints yourself:

```sh
openssl x509 -in ~/.lnx/ingress/ca/lnx-ca.crt -text -noout
```

## Removing everything

`lnx ingress disable` stops the listeners and removes the resolver and
launchd service, but leaves the CA trusted so a later re-enable does not
prompt for authorization again.

To remove the CA and all ingress state as well:

```sh
sudo lnx ingress uninstall
```

This deletes the launchd service, the resolver file, the trusted CA from the
System keychain, and the CA/certificate state under `~/.lnx/ingress`.

To remove lnx entirely: run `sudo lnx ingress uninstall` (if you ever enabled
ingress), then delete `~/.lnx` and the `lnx` binary.

## Reporting

Please report suspected vulnerabilities via GitHub security advisories on
[semistrict/lnx](https://github.com/semistrict/lnx/security/advisories) rather
than public issues.
