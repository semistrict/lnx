CC_LINUX ?= /opt/homebrew/bin/aarch64-linux-musl-gcc
CARGO ?= cargo
CODESIGN ?= codesign
BIN := target/debug/lnx
RELEASE_BIN := target/release/lnx

.PHONY: all build cargo-build sign release release-build release-sign run apt-update deps check test fmt clean

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

fmt:
	$(CARGO) fmt

clean:
	$(CARGO) clean
