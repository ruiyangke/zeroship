// Copyright (c) HashiCorp, Inc.
// SPDX-License-Identifier: MPL-2.0
//
// Scaffold-level scenario test for RecoverTask. Today every case returns the
// "T-4 not implemented" error; this file's purpose is to enumerate the cases
// so when T-4 lands, the test author has the matrix already written down.

package tests

import (
	"strings"
	"testing"

	"github.com/hashicorp/go-hclog"
	"github.com/hashicorp/nomad/plugins/drivers"

	"github.com/zeroship/nomad-driver-ch/ch"
)

// TestRecoverTask_NilHandle is the one case the stub already gets right.
func TestRecoverTask_NilHandle(t *testing.T) {
	p := ch.NewPlugin(hclog.NewNullLogger())
	err := p.RecoverTask(nil)
	if err == nil {
		t.Fatal("RecoverTask(nil) should error")
	}
	if !strings.Contains(err.Error(), "nil handle") {
		t.Errorf("unexpected error: %v", err)
	}
}

// TestRecoverTask_StubReturnsNotImplemented is the contract we promise the
// implementer of T-4: today every non-nil-handle path errors with the T-4
// tag. When T-4 lands and this test starts failing, that's the signal to
// rewrite it as the real scenario matrix below.
func TestRecoverTask_StubReturnsNotImplemented(t *testing.T) {
	p := ch.NewPlugin(hclog.NewNullLogger())
	h := drivers.NewTaskHandle(ch.TaskHandleVersion)
	h.Config = &drivers.TaskConfig{ID: "scaffold-test-id"}
	if err := h.SetDriverState(&ch.TaskState{CHPid: 1, APISocket: "/dev/null", VMIndex: 7, Tap: "zsbx-nm-7", Mode: "cold_boot"}); err != nil {
		t.Fatalf("SetDriverState: %v", err)
	}
	err := p.RecoverTask(h)
	if err == nil {
		t.Fatal("scaffold should error")
	}
	if !strings.Contains(err.Error(), "T-4") {
		t.Errorf("scaffold should be tagged T-4, got: %v", err)
	}
}

// --- Cases below this line are the matrix T-4 must satisfy. Each is skipped
// until the implementer wires it up. The implementer should:
//   1. Remove the t.Skip();
//   2. Replace the body with the assertion appropriate to the case;
//   3. Run go test ./... and confirm it passes.
//
// Until then they exist as living TODOs the test framework will count.

func TestRecoverTask_ProcessAliveSocketResponsive(t *testing.T) {
	t.Skip("T-4: implement when RecoverTask is wired up — expect ok + handle in p.tasks")
}

func TestRecoverTask_ProcessAliveSocketDead(t *testing.T) {
	t.Skip("T-4: implement when RecoverTask is wired up — expect drivers.ErrTaskNotFound")
}

func TestRecoverTask_ProcessGone(t *testing.T) {
	t.Skip("T-4: implement when RecoverTask is wired up — expect drivers.ErrTaskNotFound")
}

func TestRecoverTask_ProcessAliveButNotCH(t *testing.T) {
	t.Skip("T-4: implement when RecoverTask is wired up — expect drivers.ErrTaskNotFound (pid reused)")
}

func TestRecoverTask_DoubleCallIsIdempotent(t *testing.T) {
	t.Skip("T-4: implement when RecoverTask is wired up — expect second call is a no-op")
}
