// Copyright (c) HashiCorp, Inc.
// SPDX-License-Identifier: MPL-2.0
//
// TestMain hooks for the tests/ package. Currently a single concern:
// disable the driver-metrics file exporter goroutine by default so
// 30+ NewPlugin calls across the suite don't each spawn a goroutine
// trying to MkdirAll /var/lib/zsbx under an unprivileged user.
//
// Tests that need the exporter explicitly re-enable it via
// SetDriverMetricsEnabledForTest(true) + RunDriverMetricsExporterForTest
// — see metrics_exporter_test.go for the pattern.

package tests

import (
	"os"
	"testing"

	"github.com/zeroship/nomad-driver-ch/ch"
)

func TestMain(m *testing.M) {
	// T-8b-stress-r8 r7-B: gate the exporter goroutine off by
	// default so test-binary lifetime doesn't accumulate leaked
	// goroutines. Per-test re-enable for the exporter test cases.
	ch.SetDriverMetricsEnabledForTest(false)
	os.Exit(m.Run())
}
