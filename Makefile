.PHONY: all cmd/lnx/init lnx lnx-linux kernel rootfs test test-integration test-memorysnapshot install deps-macos clean help

all: lnx

help:
	@echo "Usage: make [target]"
	@echo ""
	@echo "Build:"
	@echo "  all              Build the lnx binary (default)"
	@echo "  lnx              Build the lnx binary with codesign (macOS)"
	@echo "  lnx-linux        Build the lnx binary for Linux/arm64"
	@echo "  install          Install to \$$GOPATH/bin"
	@echo "  kernel           Build the Linux kernel in Docker"
	@echo "  rootfs           Build the ext4 rootfs image in Docker"
	@echo ""
	@echo "Test:"
	@echo "  test             Run unit tests (any platform)"
	@echo "  test-integration Run integration tests (macOS, optional TEST=regex filter)"
	@echo "  test-memorysnapshot Run focused memory snapshot integration tests (optional TEST=regex filter)"
	@echo ""
	@echo "Other:"
	@echo "  deps-macos       Install local macOS dependencies (currently zstd)"
	@echo "  clean            Remove build artifacts"
	@echo "  help             Show this help"

# Cross-compile guest init binary (linux/arm64)
cmd/lnx/init:
	CGO_ENABLED=0 GOOS=linux GOARCH=arm64 go build -trimpath -o $@ ./cmd/init

# Build host binary with embedded init
lnx: cmd/lnx/init
	go build -ldflags '-extldflags "-Wl,-no_warn_duplicate_libraries"' -o $@ ./cmd/lnx
	codesign --entitlements entitlements.plist --force -s - $@

# Build Linux binary (no codesign needed)
lnx-linux: cmd/lnx/init
	CGO_ENABLED=0 GOOS=linux GOARCH=arm64 go build -trimpath -o $@ ./cmd/lnx

# Build kernel in Docker
kernel:
	docker build --platform linux/arm64 -f Dockerfile.kernel -t lnx-kernel .
	docker rm -f lnx-kernel-extract 2>/dev/null; true
	docker create --name lnx-kernel-extract lnx-kernel true
	docker cp lnx-kernel-extract:/build/arch/arm64/boot/Image vmlinuz
	docker rm lnx-kernel-extract
	@echo "Kernel built: vmlinuz"

# Build rootfs ext4 image in Docker
rootfs:
	docker build --platform linux/arm64 -f Dockerfile.rootfs -t lnx-rootfs .
	docker rm -f lnx-rootfs-extract 2>/dev/null; true
	docker create --name lnx-rootfs-extract lnx-rootfs true
	docker cp lnx-rootfs-extract:/rootfs.ext4 rootfs.ext4
	docker rm lnx-rootfs-extract
	@echo "Rootfs built: rootfs.ext4"

# Unit tests (run anywhere)
test:
	go test -v ./...

# Integration tests (macOS only, needs kernel+rootfs+init in place)
# Usage: make test-integration [TEST=Regex]
test-integration: lnx
	go build -o /tmp/lnx-codesign ./cmd/codesign
	PATH="$(PWD):$$PATH" go test -v -timeout 180s -tags integration -exec /tmp/lnx-codesign $(if $(TEST),-run '$(TEST)') ./...

test-memorysnapshot: lnx
	go build -o /tmp/lnx-codesign ./cmd/codesign
	PATH="$(PWD):$$PATH" LNX_EXPERIMENTS=memorysnapshot go test -v -timeout 180s -tags integration -exec /tmp/lnx-codesign $(if $(TEST),-run '$(TEST)',-run 'TestCLI_MemoryClone(PreservesProcessState|PreservesLnxControlState)$$') ./...

# Install to $GOPATH/bin
install: cmd/lnx/init
	go build -ldflags '-extldflags "-Wl,-no_warn_duplicate_libraries"' -o "$$(go env GOPATH)/bin/lnx" ./cmd/lnx
	codesign --entitlements entitlements.plist --force -s - "$$(go env GOPATH)/bin/lnx"

# Install local macOS dependencies used by lnx.
deps-macos:
	brew install zstd

clean:
	rm -f lnx lnx-linux cmd/lnx/init vmlinuz vmlinuz.gz rootfs.ext4
