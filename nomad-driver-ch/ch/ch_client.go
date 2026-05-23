// Copyright (c) HashiCorp, Inc.
// SPDX-License-Identifier: MPL-2.0
//
// New file (no upstream equivalent). The libvirt provider that lived under
// providers/libvirt/ in nomad-driver-virt is replaced wholesale by this
// CH-specific client; the wire idiom (Unix-domain HTTP) is documented in the
// volantvm spike § 3 ("vm_operations.go::httpRequest") and the proposal § 7.

package ch

import (
	"context"
	"errors"
	"net"
	"net/http"
	"time"

	"github.com/hashicorp/go-hclog"
)

// Client is a thin wrapper around `cloud-hypervisor` (as a subprocess) and
// the `ch-remote` REST API exposed over the per-VM Unix socket. It does NOT
// hold per-task state — that lives on taskHandle. The single shared Client
// holds the HTTP transport and the logger.
//
// The CH REST shape we target (per the volantvm spike and the CH OpenAPI
// spec at vmm/src/api/openapi/cloud-hypervisor.yaml):
//
//   PUT  /api/v1/vm.create   { payload: { kernel, cmdline, initramfs? },
//                              cpus, memory, disks, net, fs, ... }
//   PUT  /api/v1/vm.boot     {}
//   PUT  /api/v1/vm.resume   {}             // post-restore
//   PUT  /api/v1/vm.shutdown {}             // graceful stop
//   GET  /api/v1/vm.info                    // for TaskStats + InspectTask
//   PUT  /api/v1/vm.snapshot { destination_url: "file://<path>" }
//   PUT  /api/v1/vm.restore  { source_url:      "file://<path>" }
//
// Per-call socket dial is fine — the calls are not in any hot path; CH
// keeps the listener open for the life of the VM.
type Client struct {
	logger hclog.Logger
}

// NewClient constructs the shared Client. Today it only stashes the logger;
// once T-1 lands, it will also resolve the absolute paths to `cloud-hypervisor`
// and `ch-remote` (from the driver Config) and verify they exist.
func NewClient(logger hclog.Logger) *Client {
	return &Client{logger: logger}
}

// httpClientForSocket returns an http.Client whose Transport dials the given
// Unix-domain socket path on every request. Hostname in the URL is ignored
// by the dialer; convention is "unix" (matches the upstream virt + volantvm
// idiom).
//
// Kept here as a helper that's exercised by tests once T-1 starts emitting
// requests. The 5-second timeout is the volantvm value and works fine for
// our cold-boot envelope.
func httpClientForSocket(socketPath string) *http.Client {
	return &http.Client{
		Timeout: 5 * time.Second,
		Transport: &http.Transport{
			DialContext: func(_ context.Context, _, _ string) (net.Conn, error) {
				return net.Dial("unix", socketPath)
			},
		},
	}
}

// Info will return the parsed `vm.info` JSON for the VM at socketPath.
// Stubbed until T-1 needs it.
func (c *Client) Info(socketPath string) (*VMInfo, error) {
	return nil, errors.New("ch: T-1: Client.Info not implemented")
}

// Shutdown sends `PUT /api/v1/vm.shutdown` to the VM at socketPath.
// Stubbed until T-2 wires up the graceful-stop ladder.
func (c *Client) Shutdown(socketPath string) error {
	return errors.New("ch: T-2: Client.Shutdown not implemented")
}

// Resume sends `PUT /api/v1/vm.resume` to the VM at socketPath.
// Stubbed until T-2 implements the restore path.
func (c *Client) Resume(socketPath string) error {
	return errors.New("ch: T-2: Client.Resume not implemented")
}

// VMInfo is the decoded shape of CH's `GET /api/v1/vm.info` payload. Only
// the fields we consume are listed; CH's actual response is larger but
// stable across v48..v51 (per the volantvm spike § 2 row 9).
type VMInfo struct {
	State  string `json:"state"` // "Created"|"Running"|"Shutdown"|...
	Memory struct {
		ActualSize uint64 `json:"actual_size"` // bytes
	} `json:"memory"`
	CPU struct {
		Utilisation uint64 `json:"utilisation"`
	} `json:"cpu"`
}
