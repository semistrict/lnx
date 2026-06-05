CC_LINUX ?= /opt/homebrew/bin/aarch64-linux-musl-gcc
CARGO ?= cargo
CODESIGN ?= codesign
BIN := target/debug/lnx
RELEASE_BIN := target/release/lnx
INSTALL_BIN ?= $(HOME)/.cargo/bin

.PHONY: all build cargo-build sign release release-build release-sign install run apt-update deps check test test-system rootfs fmt clean

all: build

cargo-build:
	CC_LINUX=$(CC_LINUX) $(CARGO) build

sign: cargo-build
	$(CODESIGN) --entitlements entitlements.plist --force -s - $(BIN)

build: sign

release-build:
	CC_LINUX=$(CC_LINUX) $(CARGO) build --release

release-sign: release-build
	$(CODESIGN) --entitlements entitlements.plist --force -s - $(RELEASE_BIN)

release: release-sign

install: release
	mkdir -p $(INSTALL_BIN)
	install -m 755 $(RELEASE_BIN) $(INSTALL_BIN)/lnx

run: build
	$(BIN) /bin/echo hello

apt-update: build
	$(BIN) /usr/bin/apt-get update

deps:
	brew install FiloSottile/musl-cross/musl-cross podman

check:
	CC_LINUX=$(CC_LINUX) $(CARGO) check

test:
	CC_LINUX=$(CC_LINUX) $(CARGO) test

test-system: build
	LNX_BIN=$(BIN) scripts/system-test.sh

rootfs:
	docker buildx build --platform linux/arm64 -f Dockerfile.rootfs -t lnx-rootfs --load .
	docker rm -f lnx-rootfs-extract >/dev/null 2>&1 || true
	docker create --name lnx-rootfs-extract lnx-rootfs true
	docker cp lnx-rootfs-extract:/rootfs.ext4 rootfs.ext4
	docker rm lnx-rootfs-extract
	scripts/prepare-rootfs-image.sh rootfs.ext4

fmt:
	$(CARGO) fmt

clean:
	$(CARGO) clean
