// Copyright (c) HashiCorp, Inc.
// SPDX-License-Identifier: MPL-2.0
//
// T-5 sprint test surface: pins the TaskStats matrix — ticked emission of
// TaskResourceUsage with monotonically advancing timestamps + non-negative
// CPU deltas, channel-close on ctx cancel and on task-exit (exitDone +
// processAlive seam), tolerance of intermittent /proc errors, and the
// unknown-taskID immediate-error path.
//
// All tests use the package-level seams in ch/ (statsCollectorFn,
// processAliveFn) so no real /proc reads happen — the suite is hermetic.

package tests

import (
	"context"
	"errors"
	"fmt"
	"sync/atomic"
	"testing"
	"time"

	"github.com/hashicorp/go-hclog"

	"github.com/zeroship/nomad-driver-ch/ch"
)

// installStatsCollectorSeam overrides the package-level statsCollectorFn
// with fn and restores the previous fn on test cleanup. Mirrors the
// installProbeSeam helper in recover_task_test.go.
func installStatsCollectorSeam(t *testing.T, fn func(pid int) (*ch.HostStatsSeam, error)) {
	t.Helper()
	prev := ch.SetStatsCollectorForTest(fn)
	t.Cleanup(func() { ch.SetStatsCollectorForTest(prev) })
}

// installAliveSeamLocal mirrors the helper from recover_task_test.go.
// Local copy to avoid cross-file ordering assumptions (the recover_task
// helper accepts no fn; this one accepts the fake to drive task-gone).
func installAliveSeamLocal(t *testing.T, fn func(int) error) {
	t.Helper()
	prev := ch.SetProcessAliveForTest(fn)
	t.Cleanup(func() { ch.SetProcessAliveForTest(prev) })
}

// TestTaskStats_EmitsResourceUsageAtInterval — the happy path. The fake
// counter source returns monotonically increasing CPU + RSS; the test
// waits for N ticks at a 10 ms interval and asserts:
//
//   - each TaskResourceUsage has a non-zero Timestamp;
//   - the timestamps are monotonically non-decreasing;
//   - the cumulative TotalTicks on later samples is >= earlier ones
//     (we don't pin Percent because the test interval and userHZ
//     interact; TotalTicks is the cumulative truth).
//   - RSS reflects the latest fake sample.
func TestTaskStats_EmitsResourceUsageAtInterval(t *testing.T) {
	// Make CPU ticks monotonically grow; vary RSS so we can witness
	// the latest sample landing in MemoryStats.
	var calls atomic.Int32
	installStatsCollectorSeam(t, func(pid int) (*ch.HostStatsSeam, error) {
		n := uint64(calls.Add(1))
		// utime grows by 5 per tick, stime by 3 per tick — both
		// monotonic so the safeSub clamp doesn't fire.
		return ch.NewHostStatsForTest(5*n, 3*n, 1024*1024*n), nil
	})
	installAliveSeamLocal(t, func(int) error { return nil })

	p := ch.NewPlugin(hclog.NewNullLogger()).(*ch.Plugin)
	_ = ch.InstallFakeRunningTaskForStats(p, "stats-task", 4242)

	ctx, cancel := context.WithTimeout(context.Background(), 500*time.Millisecond)
	defer cancel()

	stream, err := p.TaskStats(ctx, "stats-task", 10*time.Millisecond)
	if err != nil {
		t.Fatalf("TaskStats: %v", err)
	}

	want := 3
	got := make([]*driversTaskResourceUsage, 0, want)
	deadline := time.After(2 * time.Second)
	for len(got) < want {
		select {
		case usage, ok := <-stream:
			if !ok {
				t.Fatalf("stream closed before receiving %d samples; got %d", want, len(got))
			}
			got = append(got, usage)
		case <-deadline:
			t.Fatalf("did not receive %d samples within 2s; got %d", want, len(got))
		}
	}

	for i, u := range got {
		if u.Timestamp == 0 {
			t.Errorf("sample %d: Timestamp = 0", i)
		}
		if u.ResourceUsage == nil || u.ResourceUsage.CpuStats == nil || u.ResourceUsage.MemoryStats == nil {
			t.Fatalf("sample %d: ResourceUsage shape malformed: %+v", i, u)
		}
		if u.Pids == nil || len(u.Pids) != 1 {
			t.Errorf("sample %d: Pids = %v, want one entry", i, u.Pids)
		}
		if _, ok := u.Pids["4242"]; !ok {
			t.Errorf("sample %d: Pids missing chPid 4242: %v", i, u.Pids)
		}
	}

	// Monotonic timestamps.
	for i := 1; i < len(got); i++ {
		if got[i].Timestamp < got[i-1].Timestamp {
			t.Errorf("Timestamps not monotonic: got[%d]=%d < got[%d]=%d",
				i, got[i].Timestamp, i-1, got[i-1].Timestamp)
		}
	}

	// Monotonic TotalTicks (the cumulative CPU-ticks counter). First
	// sample's TotalTicks is 0 (no prev sample yet); subsequent
	// samples must be >= the previous.
	for i := 2; i < len(got); i++ {
		if got[i].ResourceUsage.CpuStats.TotalTicks < got[i-1].ResourceUsage.CpuStats.TotalTicks {
			t.Errorf("TotalTicks regressed at sample %d: %f < %f",
				i,
				got[i].ResourceUsage.CpuStats.TotalTicks,
				got[i-1].ResourceUsage.CpuStats.TotalTicks)
		}
	}

	// CPU deltas must be non-negative — confirms safeSub clamp.
	for i := 1; i < len(got); i++ {
		if got[i].ResourceUsage.CpuStats.UserMode < 0 {
			t.Errorf("UserMode < 0 at sample %d: %f", i, got[i].ResourceUsage.CpuStats.UserMode)
		}
		if got[i].ResourceUsage.CpuStats.SystemMode < 0 {
			t.Errorf("SystemMode < 0 at sample %d: %f", i, got[i].ResourceUsage.CpuStats.SystemMode)
		}
	}

	// RSS reflects the fake's latest value. The fake returns
	// 1 MiB * call_index; the last sample's RSS should be >= the
	// second-to-last (monotonic).
	if got[len(got)-1].ResourceUsage.MemoryStats.RSS < got[0].ResourceUsage.MemoryStats.RSS {
		t.Errorf("RSS regressed: last=%d, first=%d",
			got[len(got)-1].ResourceUsage.MemoryStats.RSS,
			got[0].ResourceUsage.MemoryStats.RSS)
	}
}

// TestTaskStats_ChannelClosesOnContextCancel — start the collector, cancel
// ctx, expect the channel to close within a tight bound. We do not check
// for any particular number of emissions — that is interval-dependent —
// just that the close happens promptly.
func TestTaskStats_ChannelClosesOnContextCancel(t *testing.T) {
	installStatsCollectorSeam(t, func(pid int) (*ch.HostStatsSeam, error) {
		return ch.NewHostStatsForTest(1, 1, 1<<20), nil
	})
	installAliveSeamLocal(t, func(int) error { return nil })

	p := ch.NewPlugin(hclog.NewNullLogger()).(*ch.Plugin)
	_ = ch.InstallFakeRunningTaskForStats(p, "cancel-task", 5050)

	ctx, cancel := context.WithCancel(context.Background())
	stream, err := p.TaskStats(ctx, "cancel-task", 10*time.Millisecond)
	if err != nil {
		t.Fatalf("TaskStats: %v", err)
	}

	// Consume one sample so we know the goroutine is alive.
	select {
	case <-stream:
	case <-time.After(500 * time.Millisecond):
		t.Fatal("did not get first sample within 500ms")
	}

	cancel()

	// Drain until close; bounded by a generous deadline.
	deadline := time.After(500 * time.Millisecond)
	for {
		select {
		case _, ok := <-stream:
			if !ok {
				return // channel closed — success
			}
		case <-deadline:
			t.Fatal("channel did not close within 500ms of ctx.Cancel")
		}
	}
}

// TestTaskStats_ChannelClosesOnTaskExit — flip processAliveFn from
// "alive" to "gone" and assert the collector exits + closes the channel.
// This is the defence-in-depth gate; the same close also happens when
// exitDone is closed (see TestTaskStats_ChannelClosesOnExitDone).
func TestTaskStats_ChannelClosesOnTaskExit(t *testing.T) {
	installStatsCollectorSeam(t, func(pid int) (*ch.HostStatsSeam, error) {
		return ch.NewHostStatsForTest(1, 1, 1<<20), nil
	})

	var aliveCalls atomic.Int32
	installAliveSeamLocal(t, func(int) error {
		if aliveCalls.Add(1) <= 2 {
			return nil
		}
		return errors.New("ch: process is gone")
	})

	p := ch.NewPlugin(hclog.NewNullLogger()).(*ch.Plugin)
	_ = ch.InstallFakeRunningTaskForStats(p, "exit-task", 6060)

	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()
	stream, err := p.TaskStats(ctx, "exit-task", 10*time.Millisecond)
	if err != nil {
		t.Fatalf("TaskStats: %v", err)
	}

	deadline := time.After(1 * time.Second)
	for {
		select {
		case _, ok := <-stream:
			if !ok {
				return // channel closed because PID is gone — success
			}
		case <-deadline:
			t.Fatal("channel did not close within 1s of processAlive flipping to gone")
		}
	}
}

// TestTaskStats_ChannelClosesOnExitDone — closing exitDone (the
// supervisor's signal that the CH process has been observed to exit)
// must also tear down the collector. Pins the canonical close path
// independent of the processAlive defence-in-depth.
func TestTaskStats_ChannelClosesOnExitDone(t *testing.T) {
	installStatsCollectorSeam(t, func(pid int) (*ch.HostStatsSeam, error) {
		return ch.NewHostStatsForTest(1, 1, 1<<20), nil
	})
	installAliveSeamLocal(t, func(int) error { return nil })

	p := ch.NewPlugin(hclog.NewNullLogger()).(*ch.Plugin)
	exitDone := ch.InstallFakeRunningTaskForStats(p, "exitdone-task", 7070)

	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()
	stream, err := p.TaskStats(ctx, "exitdone-task", 10*time.Millisecond)
	if err != nil {
		t.Fatalf("TaskStats: %v", err)
	}

	// Consume one sample to confirm the loop is running.
	select {
	case <-stream:
	case <-time.After(500 * time.Millisecond):
		t.Fatal("first sample did not arrive within 500ms")
	}

	close(exitDone)

	deadline := time.After(500 * time.Millisecond)
	for {
		select {
		case _, ok := <-stream:
			if !ok {
				return // success
			}
		case <-deadline:
			t.Fatal("channel did not close within 500ms of exitDone close")
		}
	}
}

// TestTaskStats_TolerantOfFlakyCounters — the fake source returns errors
// every other call; the collector must log + skip rather than crash, and
// successful reads in between must still emit. Asserts:
//
//   - at least one sample makes it through;
//   - the collector does not exit early when the seam errors.
func TestTaskStats_TolerantOfFlakyCounters(t *testing.T) {
	var calls atomic.Int32
	installStatsCollectorSeam(t, func(pid int) (*ch.HostStatsSeam, error) {
		n := calls.Add(1)
		if n%2 == 0 {
			return nil, fmt.Errorf("synthetic flake on call %d", n)
		}
		return ch.NewHostStatsForTest(uint64(n), uint64(n), 1<<20), nil
	})
	installAliveSeamLocal(t, func(int) error { return nil })

	p := ch.NewPlugin(hclog.NewNullLogger()).(*ch.Plugin)
	_ = ch.InstallFakeRunningTaskForStats(p, "flake-task", 8080)

	ctx, cancel := context.WithTimeout(context.Background(), 1*time.Second)
	defer cancel()
	stream, err := p.TaskStats(ctx, "flake-task", 5*time.Millisecond)
	if err != nil {
		t.Fatalf("TaskStats: %v", err)
	}

	// Collect at least 3 samples to prove the loop survives multiple
	// flake interleavings.
	want := 3
	got := 0
	deadline := time.After(2 * time.Second)
	for got < want {
		select {
		case _, ok := <-stream:
			if !ok {
				t.Fatalf("stream closed early; got %d samples, want %d", got, want)
			}
			got++
		case <-deadline:
			t.Fatalf("did not get %d samples within 2s; got %d", want, got)
		}
	}
}

// TestTaskStats_UnknownTaskID — caller passes a taskID not in the
// driver's handle map; TaskStats must return drivers.ErrTaskNotFound
// immediately, without spawning a goroutine or returning a stream.
func TestTaskStats_UnknownTaskID(t *testing.T) {
	p := ch.NewPlugin(hclog.NewNullLogger()).(*ch.Plugin)

	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	stream, err := p.TaskStats(ctx, "no-such-task", 100*time.Millisecond)
	if err == nil {
		t.Fatal("TaskStats with unknown taskID should error")
	}
	if stream != nil {
		t.Errorf("stream should be nil on error; got %v", stream)
	}
	if !errors.Is(err, driversErrTaskNotFound) {
		t.Errorf("err = %v, want drivers.ErrTaskNotFound", err)
	}
}

// TestTaskStats_FirstSamplePopulatesMemory — even on the very first
// sample (no prev) the MemoryStats must be populated; only CpuStats
// fields are zero on the first tick. This pins the "no prev means no
// CPU delta" branch of buildResourceUsage.
func TestTaskStats_FirstSamplePopulatesMemory(t *testing.T) {
	const wantRSS = uint64(123 * 1024 * 1024)
	installStatsCollectorSeam(t, func(pid int) (*ch.HostStatsSeam, error) {
		return ch.NewHostStatsForTest(7, 3, wantRSS), nil
	})
	installAliveSeamLocal(t, func(int) error { return nil })

	p := ch.NewPlugin(hclog.NewNullLogger()).(*ch.Plugin)
	_ = ch.InstallFakeRunningTaskForStats(p, "first-task", 9090)

	ctx, cancel := context.WithTimeout(context.Background(), 200*time.Millisecond)
	defer cancel()
	stream, err := p.TaskStats(ctx, "first-task", 10*time.Millisecond)
	if err != nil {
		t.Fatalf("TaskStats: %v", err)
	}

	select {
	case u, ok := <-stream:
		if !ok {
			t.Fatal("stream closed before first sample")
		}
		if u.ResourceUsage.MemoryStats.RSS != wantRSS {
			t.Errorf("first MemoryStats.RSS = %d, want %d", u.ResourceUsage.MemoryStats.RSS, wantRSS)
		}
		// First sample: no prev → CPU deltas all zero.
		if u.ResourceUsage.CpuStats.UserMode != 0 {
			t.Errorf("first CpuStats.UserMode = %f, want 0", u.ResourceUsage.CpuStats.UserMode)
		}
		if u.ResourceUsage.CpuStats.SystemMode != 0 {
			t.Errorf("first CpuStats.SystemMode = %f, want 0", u.ResourceUsage.CpuStats.SystemMode)
		}
		if u.ResourceUsage.CpuStats.TotalTicks != 0 {
			t.Errorf("first CpuStats.TotalTicks = %f, want 0", u.ResourceUsage.CpuStats.TotalTicks)
		}
	case <-time.After(500 * time.Millisecond):
		t.Fatal("first sample did not arrive within 500ms")
	}
}
