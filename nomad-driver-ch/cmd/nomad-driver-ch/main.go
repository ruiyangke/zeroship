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
package main

import (
	"github.com/hashicorp/go-hclog"
	"github.com/hashicorp/nomad/plugins"

	"github.com/zeroship/nomad-driver-ch/ch"
)

func main() {
	plugins.Serve(factory)
}

// factory is called once by plugins.Serve with the Nomad-supplied logger and
// returns the driver instance that will field RPCs for this plugin process.
func factory(log hclog.Logger) interface{} {
	return ch.NewPlugin(log)
}
