// Copyright (c) HashiCorp, Inc.
// SPDX-License-Identifier: MPL-2.0
//
// Test-only exported names so the `tests/` package (which lives outside
// the `ch` package and therefore can't reach unexported symbols) can
// drive StartTask through the processRunner seam, exercise
// buildConfigJSON in isolation, and assert against the sentinel errors.
//
// File naming note: this is intentionally NOT `*_test.go` so the
// external test package can import these names. The compile-time
// guarantee that these are test-only is provided by the fact that they
// are documented as such and only used from the tests/ directory.

package ch

import (
	"os/exec"

	"github.com/hashicorp/go-hclog"
)

// ProcessRunnerSeam is the exported alias of processRunner, used by the
// tests/ package to define fake runners against the same contract
// StartTask consumes.
type ProcessRunnerSeam = processRunner

// BuildConfigJSON is the test entry point that exercises the cold-boot
// validation + config.json marshalling in isolation. Pure function; no
// side effects.
func BuildConfigJSON(cfg TaskConfig, taskDir string) ([]byte, error) {
	return buildConfigJSON(cfg, taskDir)
}

// NewPluginForTest constructs a *Plugin with a caller-supplied runner
// factory. Mirrors NewPlugin but lets the test substitute the fake
// runner that records argv without spawning CH.
//
// Tests should still call SetConfig (or rely on the post-NewClient env
// var resolution) to wire up the binary paths.
func NewPluginForTest(logger hclog.Logger, factory func(cmd *exec.Cmd) ProcessRunnerSeam) *Plugin {
	p := NewPlugin(logger).(*Plugin)
	if factory != nil {
		p.chClient.SetRunnerFactory(func(cmd *exec.Cmd) processRunner {
			return factory(cmd)
		})
	}
	return p
}

// ErrExistingTaskErr is a re-export of ErrExistingTask under a name that
// reads naturally in errors.Is checks ("is err the existing-task err?").
// Tests can also compare against ch.ErrExistingTask directly; this is a
// belt-and-braces alias.
var ErrExistingTaskErr = ErrExistingTask

// SetEnsureTapUpForTest replaces the tap-up seam so tests can skip the
// `ip link set up` exec (which requires CAP_NET_ADMIN). Returns the
// previous fn so the test can restore it on cleanup.
func SetEnsureTapUpForTest(fn func(tapName string) error) func(string) error {
	prev := ensureTapUpFn
	if fn != nil {
		ensureTapUpFn = fn
	}
	return prev
}
