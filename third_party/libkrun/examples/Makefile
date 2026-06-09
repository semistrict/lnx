ARCH = $(shell uname -m)
OS = $(shell uname -s)
LDFLAGS_x86_64_Linux = -lkrun
LDFLAGS_aarch64_Linux = -lkrun
LDFLAGS_riscv64_Linux = -lkrun
LDFLAGS_arm64_Darwin = -L/opt/homebrew/lib -lkrun
LDFLAGS_sev = -lkrun-sev
LDFLAGS_tdx = -lkrun-tdx
LDFLAGS_nitro = -lkrun-awsnitro
CFLAGS = -O2 -g -I../include
ROOTFS_DISTRO := fedora
ROOTFS_DIR = rootfs_$(ROOTFS_DISTRO)

.DEFAULT_GOAL := krun

.PHONY: clean rootfs krun

EXAMPLES := boot_efi chroot_vm external_kernel consoles snapshot_demo
ifeq ($(SEV),1)
    EXAMPLES := launch-tee
endif
ifeq ($(TDX),1)
    EXAMPLES := launch-tee
endif

all: $(EXAMPLES)

krun:
	rustup target add aarch64-unknown-linux-musl
	cargo build -p krun --bin krun --release
	rm -rf "$$HOME/.libkrun"/krun-snapshot-*
	cp target/release/krun krun/krun
ifeq ($(OS),Darwin)
	codesign --entitlements chroot_vm.entitlements --timestamp=none --force -s - krun/krun
endif

chroot_vm: chroot_vm.c
	gcc -o $@ $< $(CFLAGS) $(LDFLAGS_$(ARCH)_$(OS))
ifeq ($(OS),Darwin)
	codesign --entitlements chroot_vm.entitlements --force -s - $@
endif

launch-tee: launch-tee.c
ifeq ($(SEV),1)
	gcc -o $@ $< $(CFLAGS) $(LDFLAGS_sev)
endif
ifeq ($(TDX),1)
	gcc -o $@ $< $(CFLAGS) $(LDFLAGS_tdx)
endif

boot_efi: boot_efi.c
	gcc -o $@ $< $(CFLAGS) $(LDFLAGS_$(ARCH)_$(OS))
ifeq ($(OS),Darwin)
	codesign --entitlements chroot_vm.entitlements --force -s - $@
endif

external_kernel: external_kernel.c
	gcc -o $@ $< $(CFLAGS) $(LDFLAGS_$(ARCH)_$(OS))
ifeq ($(OS),Darwin)
	codesign --entitlements chroot_vm.entitlements --force -s - $@
endif

consoles: consoles.c
	gcc -o $@ $< $(CFLAGS) $(LDFLAGS_$(ARCH)_$(OS))

snapshot_demo: snapshot_demo.c
	gcc -o $@ $< $(CFLAGS) -lpthread $(LDFLAGS_$(ARCH)_$(OS))
ifeq ($(OS),Darwin)
	codesign --entitlements chroot_vm.entitlements --force -s - $@
endif

nitro: nitro.c
	gcc -o $@ $< $(CFLAGS) $(LDFLAGS_nitro)

# Build the rootfs to be used with chroot_vm.
rootfs:
	mkdir -p $(ROOTFS_DIR)
	podman create --name libkrun_chroot_vm $(ROOTFS_DISTRO)
	podman export libkrun_chroot_vm | tar xpf - -C $(ROOTFS_DIR)
	podman rm libkrun_chroot_vm

clean:
	rm -rf chroot_vm $(ROOTFS_DIR) launch-tee boot_efi external_kernel nitro consoles krun/krun
