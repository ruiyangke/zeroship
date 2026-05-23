# Copyright (c) HashiCorp, Inc.
# SPDX-License-Identifier: MPL-2.0
#
# Adapted from hashicorp/nomad-driver-virt GNUmakefile on 2026-05-25.
# CGO is disabled (no libvirt), targets are pure-Go.

PLUGIN_BINARY := nomad-driver-ch
GO_MODULE     := github.com/zeroship/nomad-driver-ch
BIN_DIR       := $(CURDIR)/bin

# We don't link against libvirt; pure-Go everywhere keeps the binary
# trivially static-linkable and cross-compilable.
export CGO_ENABLED := 0

.PHONY: default
default: vet test build

.PHONY: build
build: ## Compile the plugin binary into bin/
	@mkdir -p $(BIN_DIR)
	go build -o $(BIN_DIR)/$(PLUGIN_BINARY) ./cmd/$(PLUGIN_BINARY)

.PHONY: test
test: ## Run all tests (stubs may fail; compilation must succeed)
	go test ./...

.PHONY: vet
vet: ## Static analysis via go vet
	go vet ./...

.PHONY: lint
lint: ## Static analysis via golangci-lint, if installed
	@if command -v golangci-lint >/dev/null 2>&1; then \
		golangci-lint run ./...; \
	else \
		echo "golangci-lint not installed; skipping (install: https://golangci-lint.run)"; \
	fi

.PHONY: tidy
tidy: ## Tidy go.mod / go.sum
	go mod tidy

.PHONY: clean
clean: ## Remove build artefacts
	rm -rf $(BIN_DIR)

.PHONY: plugin-package
plugin-package: build ## Produce a tarball suitable for /opt/nomad/plugins
	tar -C $(BIN_DIR) -czf $(BIN_DIR)/$(PLUGIN_BINARY).tar.gz $(PLUGIN_BINARY)

.PHONY: help
help: ## Show this help
	@awk 'BEGIN {FS = ":.*?## "} /^[a-zA-Z_-]+:.*?## / {printf "  %-20s %s\n", $$1, $$2}' $(MAKEFILE_LIST)
