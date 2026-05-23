// Copyright (c) HashiCorp, Inc.
// SPDX-License-Identifier: MPL-2.0
//
// Test helpers for the tests/ package. Builds a *drivers.TaskConfig
// pointed at a t.TempDir() and msgpack-encoded with the ch.TaskConfig
// payload so StartTask's DecodeDriverConfig round-trips correctly.

package tests

import (
	"path/filepath"
	"testing"

	"github.com/hashicorp/nomad/plugins/drivers"

	"github.com/zeroship/nomad-driver-ch/ch"
)

// driversTaskConfig is an alias used by start_task_test.go so the helper
// signature reads naturally.
type driversTaskConfig = drivers.TaskConfig

// newDriversTaskConfig builds a *drivers.TaskConfig whose TaskDir()
// resolves under the supplied taskDir. The driver-config payload is
// msgpack-encoded so cfg.DecodeDriverConfig recovers a ch.TaskConfig
// identical to driverCfg.
//
// Nomad computes TaskDir() as filepath.Join(AllocDir, Name); we set
// AllocDir to the parent of taskDir and Name to its basename so
// TaskDir().Dir == taskDir. The driver's taskRunDir picks LocalDir =
// taskDir/local; we don't pre-create that — StartTask MkdirAlls it.
func newDriversTaskConfig(t *testing.T, driverCfg *ch.TaskConfig, taskDir string) *drivers.TaskConfig {
	t.Helper()
	allocDir := filepath.Dir(taskDir)
	name := filepath.Base(taskDir)
	cfg := &drivers.TaskConfig{
		ID:       "test-task-" + name,
		Name:     name,
		AllocDir: allocDir,
	}
	if err := cfg.EncodeConcreteDriverConfig(driverCfg); err != nil {
		t.Fatalf("EncodeConcreteDriverConfig: %v", err)
	}
	return cfg
}
