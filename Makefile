.PHONY: all cmd/lnx/init lnx kernel rootfs test test-integration install clean help

all: lnx

help:
	@echo "Usage: make [target]"
	@echo ""
	@echo "Build:"
	@echo "  all              Build the lnx binary (default)"
	@echo "  lnx              Build the lnx binary with codesign"
	@echo "  install          Install to \$$GOPATH/bin"
	@echo "  kernel           Build the Linux kernel in Docker"
	@echo "  rootfs           Build the ext4 rootfs image in Docker"
	@echo ""
	@echo "Test:"
	@echo "  test             Run unit tests (any platform)"
	@echo "  test-integration Run integration tests (macOS, needs kernel+rootfs)"
	@echo ""
	@echo "Other:"
	@echo "  clean            Remove build artifacts"
	@echo "  help             Show this help"

# Cross-compile guest init binary (linux/arm64)
cmd/lnx/init:
	CGO_ENABLED=0 GOOS=linux GOARCH=arm64 go build -o $@ ./cmd/init

# Build host binary with embedded init
lnx: cmd/lnx/init
	go build -ldflags '-extldflags "-Wl,-no_warn_duplicate_libraries"' -o $@ ./cmd/lnx
	codesign --entitlements entitlements.plist --force -s - $@

# Build kernel in Docker
kernel:
	docker build --platform linux/arm64 -f Dockerfile.kernel -t lnx-kernel .
	docker rm -f lnx-kernel-extract 2>/dev/null; true
	docker create --name lnx-kernel-extract lnx-kernel true
	docker cp lnx-kernel-extract:/build/arch/arm64/boot/Image kernel.Image
	docker rm lnx-kernel-extract
	@echo "Kernel built: kernel.Image"

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
# Usage: make test-integration [RUN=TestName]
test-integration: cmd/lnx/init
	go build -o /tmp/lnx-codesign ./cmd/codesign
	go test -v -timeout 180s -tags integration -exec /tmp/lnx-codesign $(if $(RUN),-run '$(RUN)') ./...

# Install to $GOPATH/bin
install: cmd/lnx/init
	go install -ldflags '-extldflags "-Wl,-no_warn_duplicate_libraries"' ./cmd/lnx
	codesign --entitlements entitlements.plist --force -s - "$$(go env GOPATH)/bin/lnx"

clean:
	rm -f lnx cmd/lnx/init kernel.Image rootfs.ext4
