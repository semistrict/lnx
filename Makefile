CC_LINUX ?= /opt/homebrew/bin/aarch64-linux-musl-gcc
CARGO ?= cargo
CODESIGN ?= codesign
BIN := target/debug/lnx
RELEASE_BIN := target/release/lnx
INSTALL_BIN ?= $(HOME)/.cargo/bin

.PHONY: all build release install sign-notarize run apt-update deps check test test-system test-checkpoint test-fork-fanout test-snapshot-compat test-snapshot-roundtrip test-dirty-fs test-broker-recovery test-client-chaos test-pty-resume test-browser test-privileged-ingress test-stress test-stock test-ingress test-longevity test-full rootfs fmt clean

all: build

build:
	bun run build

release:
	bun run release

install:
	bun run install

sign-notarize: release
	scripts/sign-notarize.sh $(RELEASE_BIN)

run: build
	$(BIN) /bin/echo hello

apt-update: build
	$(BIN) /usr/bin/apt-get update

deps:
	brew install FiloSottile/musl-cross/musl-cross podman

check:
	bun run check

test:
	bun run test

test-system:
	bun run test:system

test-checkpoint:
	bun run test:checkpoint

test-fork-fanout:
	bun run test:fork-fanout

test-snapshot-compat:
	bun run test:snapshot-compat

test-snapshot-roundtrip:
	bun run test:snapshot-roundtrip

test-dirty-fs:
	bun run test:dirty-fs

test-broker-recovery:
	bun run test:broker-recovery

test-client-chaos:
	bun run test:client-chaos

test-pty-resume:
	bun run test:pty-resume

test-browser:
	bun run test:browser

test-privileged-ingress:
	bun run test:privileged-ingress

test-stress:
	bun run test:stress

test-stock:
	bun run test:stock

test-ingress:
	bun run test:ingress

test-longevity:
	bun run test:longevity

test-full:
	bun run test:full

rootfs:
	docker buildx build --platform linux/arm64 -f Dockerfile.rootfs -t lnx-rootfs --load .
	docker rm -f lnx-rootfs-extract >/dev/null 2>&1 || true
	docker create --name lnx-rootfs-extract lnx-rootfs true
	docker cp lnx-rootfs-extract:/rootfs.ext4 rootfs.ext4
	docker rm lnx-rootfs-extract
	scripts/prepare-rootfs-image.sh rootfs.ext4

fmt:
	bun run fmt

clean:
	bun run clean
