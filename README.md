# lnx

Lightweight Linux VM runner for macOS using Apple's Virtualization.framework. Boots a Linux kernel directly (no UEFI/bootloader), runs a custom init as PID 1, communicates over vsock.

## Install

```
brew install semistrict/tap/lnx --HEAD
```

Then download or build a kernel and rootfs:

```
make kernel rootfs
lnx init --kernel vmlinuz --rootfs rootfs.ext4
```

## Usage

```
lnx                              # login shell (bash -l)
lnx python3 server.py            # run a command
lnx --ssh-agent git push         # with SSH agent forwarding
lnx --ephemeral make test        # disposable VM, rootfs discarded on exit
lnx --instance dev bash -l       # named instance
```

## Instances

Each instance gets its own rootfs, sockets, logs, and checkpoints under `~/.lnx/instances/<name>/`.

```
lnx instance create dev           # clone default rootfs
lnx instance list                  # show all instances
lnx --instance dev bash -l         # boot a specific instance
lnx instance delete dev            # remove an instance
```

## Shares

Share host directories read-write into the VM via virtiofs:

```
lnx share add ~/src               # persisted per-instance
lnx share list
lnx share remove ~/src
```

The current working directory is always shared automatically.

## Docker

Docker works out of the box after installing it in the VM:

```
sudo apt install docker-ce docker-ce-cli containerd.io
sudo systemctl enable docker
```

Docker auto-starts on boot. No `sudo` needed for `docker` commands.

## Other commands

```
lnx status                        # VM status (all running instances)
lnx ports list                    # forwarded ports
lnx expose web:8080 --as=:8081    # expose web:8080 on localhost:8081
lnx ingress enable                # install .lnx resolver and local HTTP ingress
curl http://p8080.dev.lnx/        # route to dev:8080 (VM must already be running)
lnx exec [-i] command             # exec into a running VM
lnx disk grow 16G                 # grow rootfs (resized on next boot)
lnx checkpoints list              # list rootfs checkpoints
```
