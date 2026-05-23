// Copyright (c) HashiCorp, Inc.
// SPDX-License-Identifier: MPL-2.0
//
// Forked from hashicorp/nomad-driver-virt (cmd/nomad-driver-virt/main.go shape)
// on 2026-05-25 for Cloud Hypervisor support.

// Binary nomad-driver-ch is the plugin executable Nomad launches as a child
// process. It speaks the go-plugin handshake (magic cookie, protocol version,
// gRPC server) entirely via nomad/plugins.Serve — we do NOT write the
// handshake constants ourselves. The factory returns a fresh *ch.Plugin per
// process; Nomad multiplexes individual tasks onto it.
//
// When invoked with --version (or -version / -v) the binary prints
// `nomad-driver-ch <gitSHA>` to stdout and exits 0 BEFORE handing control to
// plugins.Serve. This is the operator-facing identity probe used by the
// sandbox-side `gcp-worker-startup.sh` to confirm which build sits on disk.
// gitSHA defaults to "dev" and is overridden at link time via
// `go build -ldflags="-X main.gitSHA=<short-sha>"` (see scripts/build-binary.sh).
package main

import (
	"fmt"
	"os"

	"github.com/hashicorp/go-hclog"
	"github.com/hashicorp/nomad/plugins"

	"github.com/zeroship/nomad-driver-ch/ch"
)

// gitSHA is the short git SHA of the source tree this binary was built from.
// Overridden at link time via `-ldflags="-X main.gitSHA=<sha>"`. Stays "dev"
// for `go run`, `go test`, and `make build` so a missing override never looks
// like a real release tag.
var gitSHA = "dev"

func main() {
	if len(os.Args) > 1 {
		switch os.Args[1] {
		case "--version", "-version", "-v":
			// Format pinned by tests/version_test.go — operators grep on
			// "nomad-driver-ch" and then the gitSHA tail. Do NOT reorder.
			fmt.Printf("nomad-driver-ch %s\n", gitSHA)
			return
		}
	}
	plugins.Serve(factory)
}

// factory is called once by plugins.Serve with the Nomad-supplied logger and
// returns the driver instance that will field RPCs for this plugin process.
func factory(log hclog.Logger) interface{} {
	return ch.NewPlugin(log)
}
