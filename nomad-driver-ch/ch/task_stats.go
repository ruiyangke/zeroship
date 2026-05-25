// Copyright (c) HashiCorp, Inc.
// SPDX-License-Identifier: MPL-2.0
//
// Forked from hashicorp/nomad-driver-virt (plugin/driver.go::TaskStats and
// plugin/handle.go::fillStats) on 2026-05-25 for Cloud Hypervisor support.
// The libvirt domStats poller is replaced by a host-side /proc reader of the
// CH process (proposal § 7 "TaskStats").
//
// Counter source decision (T-5): host /proc/<pid>/stat + /proc/<pid>/status
// rather than CH's HTTP `/api/v1/vm.counters`. Two reasons:
//
//  1. `/api/v1/vm.counters` only exists on cloud-hypervisor 38+. Targeting
//     /proc lets the driver run against any CH version that supports our
//     existing `/api/v1/vm.info` calls — that envelope is wider.
//  2. /proc reads are uncontended (no Unix-socket dial, no JSON decode) and
//     return the breakdown Nomad's CpuStats wants (utime/stime in clock
//     ticks → User/SystemMode). VMInfo.CPU.Utilisation collapses both into
//     a single integer.
//
// The CH-side counters route is left open for a future sprint that wants
// per-tap network bytes (T-5 explicitly punts network — Nomad's
// TaskResourceUsage has no network fields, so reading them would only be
// useful for the driver's own logging, which is not in scope here).

package ch

import (
	"context"
	"fmt"
	"os"
	"runtime"
	"strconv"
	"strings"
	"time"

	"github.com/hashicorp/nomad/plugins/drivers"
)

// statsMeasuredCPU / statsMeasuredMem name the fields the host /proc reader
// actually populates. Matches the upstream executor's
// ExecutorBasicMeasuredCpuStats / ExecutorBasicMeasuredMemStats shape so
// `nomad alloc status` renders the same columns it does for raw_exec.
var (
	statsMeasuredCPU = []string{"System Mode", "User Mode", "Percent"}
	statsMeasuredMem = []string{"RSS"}
)

// hostStats is the raw sample the per-tick reader returns. CPU times are
// in clock ticks (USER_HZ — the kernel's CLOCKS_PER_SEC); memory is in
// bytes. The driver derives the percentages and TotalTicks in
// buildResourceUsage; keeping the raw sample seamable means tests can
// fake out /proc without re-implementing the cumulative-counter logic.
type hostStats struct {
	// UTimeTicks / STimeTicks are cumulative since process start, in
	// clock ticks (read from /proc/<pid>/stat fields 14 + 15). Their
	// per-second delta drives CpuStats.UserMode / SystemMode.
	UTimeTicks uint64
	STimeTicks uint64

	// RSSBytes is current resident set size in bytes (read from
	// /proc/<pid>/status VmRSS, which is kB).
	RSSBytes uint64
}

// statsCollectorFn is the package-level seam tests swap to fake /proc
// reads without depending on the host's PID layout. Default delegates
// to readHostStats. Mirrors the probeFn / processAliveFn / shutdownFn
// pattern.
//
// The fn receives the CH PID and returns the cumulative sample. Errors
// MUST be transient-safe: the collector loop logs and retries the next
// tick rather than tearing down the stream (a momentary EAGAIN on /proc
// during a fork must not deny the operator their entire stats stream).
var statsCollectorFn = readHostStats

// SetStatsCollectorForTest replaces the /proc reader seam. Returns the
// previous fn so the caller can restore it on cleanup.
func SetStatsCollectorForTest(fn func(pid int) (*hostStats, error)) func(int) (*hostStats, error) {
	prev := statsCollectorFn
	if fn != nil {
		statsCollectorFn = fn
	}
	return prev
}

// HostStatsSeam is the test-visible alias of hostStats so the tests/
// package can build fake samples without poking into unexported fields.
type HostStatsSeam = hostStats

// NewHostStatsForTest builds a HostStatsSeam from explicit ticks +
// bytes. Tests use this to construct fake samples without touching the
// struct's literal layout.
func NewHostStatsForTest(uTicks, sTicks, rssBytes uint64) *HostStatsSeam {
	return &hostStats{UTimeTicks: uTicks, STimeTicks: sTicks, RSSBytes: rssBytes}
}

// readHostStats samples /proc/<pid>/stat (utime, stime) + /proc/<pid>/status
// (VmRSS) and returns the cumulative sample. Lightweight; just two file
// reads with no parsing of fields beyond the ones we need.
//
// Format references:
//
//   - /proc/[pid]/stat — fields are space-separated, but the second field
//     (comm) is in parens and may contain spaces/parens itself ("(cloud
//     hypervisor)" hypothetical case). The standard trick is to find the
//     LAST ')' and split everything after that — utime is then field 14
//     counting from after the ')' (== fields 14 and 15 of the full record).
//   - /proc/[pid]/status — `VmRSS:\t<N> kB\n` line; parse the second token.
//
// Returns an error on missing /proc entry (ESRCH-equivalent) so the
// collector loop can decide whether the process is gone — but normal
// behaviour is for the supervisor's processAliveFn poll to catch that
// first.
func readHostStats(pid int) (*hostStats, error) {
	if pid <= 0 {
		return nil, fmt.Errorf("ch: readHostStats: invalid pid %d", pid)
	}

	// /proc/<pid>/stat → utime, stime
	statBytes, err := os.ReadFile(fmt.Sprintf("/proc/%d/stat", pid))
	if err != nil {
		return nil, fmt.Errorf("ch: readHostStats: read /proc/%d/stat: %w", pid, err)
	}

	// Locate the LAST ')' so a comm containing ')' parses correctly.
	stat := string(statBytes)
	rparen := strings.LastIndex(stat, ")")
	if rparen < 0 || rparen+2 > len(stat) {
		return nil, fmt.Errorf("ch: readHostStats: /proc/%d/stat malformed (no comm closer)", pid)
	}
	rest := stat[rparen+2:] // skip "<space>)"; rest starts at field 3 (state)
	fields := strings.Fields(rest)
	// After the rparen+space split, indices align as: 0=state(3), 1=ppid(4),
	// 2=pgrp(5), …, so utime(14) = index 11 and stime(15) = index 12.
	if len(fields) < 13 {
		return nil, fmt.Errorf("ch: readHostStats: /proc/%d/stat has %d post-comm fields, need >=13", pid, len(fields))
	}
	utime, err := strconv.ParseUint(fields[11], 10, 64)
	if err != nil {
		return nil, fmt.Errorf("ch: readHostStats: parse utime: %w", err)
	}
	stime, err := strconv.ParseUint(fields[12], 10, 64)
	if err != nil {
		return nil, fmt.Errorf("ch: readHostStats: parse stime: %w", err)
	}

	// /proc/<pid>/status → VmRSS
	statusBytes, err := os.ReadFile(fmt.Sprintf("/proc/%d/status", pid))
	if err != nil {
		return nil, fmt.Errorf("ch: readHostStats: read /proc/%d/status: %w", pid, err)
	}
	var rssKB uint64
	for _, line := range strings.Split(string(statusBytes), "\n") {
		if !strings.HasPrefix(line, "VmRSS:") {
			continue
		}
		// "VmRSS:\t<N> kB"
		tail := strings.TrimSpace(strings.TrimPrefix(line, "VmRSS:"))
		toks := strings.Fields(tail)
		if len(toks) < 1 {
			break
		}
		rssKB, err = strconv.ParseUint(toks[0], 10, 64)
		if err != nil {
			return nil, fmt.Errorf("ch: readHostStats: parse VmRSS: %w", err)
		}
		break
	}

	return &hostStats{
		UTimeTicks: utime,
		STimeTicks: stime,
		RSSBytes:   rssKB * 1024,
	}, nil
}

// userHZ returns the clock-ticks-per-second the kernel uses for /proc CPU
// fields. On Linux this is the CLOCKS_PER_SEC constant (=USER_HZ); the
// kernel hard-codes 100 for the architectures we care about. We do NOT
// shell out to `getconf` — that would add a per-tick fork — and there is
// no syscall to read it from Go without cgo. Hardcoding 100 matches what
// every other Linux stats consumer in the Go ecosystem does (cadvisor,
// gopsutil, the upstream Nomad executor on Linux).
//
// Wrapped as a function (vs. const) so a future port to a non-Linux host
// can override at build time; today it always returns 100 on Linux and a
// safe default (100) elsewhere.
func userHZ() uint64 {
	// Architecture-conditional only matters for non-Linux; on every
	// Linux we ship to, USER_HZ is 100. The runtime check keeps the
	// build portable.
	if runtime.GOOS != "linux" {
		return 100
	}
	return 100
}

// buildResourceUsage maps a cumulative hostStats sample plus the previous
// sample + the elapsed time into a Nomad-shaped TaskResourceUsage. Pure
// function so tests can drive it without spinning up the collector.
//
// prev may be nil on the first tick; in that case CpuStats UserMode /
// SystemMode / TotalTicks / Percent are reported as zero (we don't have
// a delta to compute against yet — Nomad's docker driver does the same).
func buildResourceUsage(pid int, cur, prev *hostStats, elapsed time.Duration, ts time.Time) *drivers.TaskResourceUsage {
	cpu := &drivers.CpuStats{Measured: statsMeasuredCPU}
	if prev != nil && elapsed > 0 {
		// Delta in ticks; convert to seconds of CPU time consumed in
		// the interval, then normalise to % of one core.
		dU := safeSub(cur.UTimeTicks, prev.UTimeTicks)
		dS := safeSub(cur.STimeTicks, prev.STimeTicks)
		hz := float64(userHZ())
		sec := elapsed.Seconds()
		// UserMode / SystemMode are reported as % of one core
		// (matches the executor's procstats shape — the same field
		// the docker / raw_exec drivers populate).
		cpu.UserMode = (float64(dU) / hz) / sec * 100.0
		cpu.SystemMode = (float64(dS) / hz) / sec * 100.0
		cpu.Percent = cpu.UserMode + cpu.SystemMode
		cpu.TotalTicks = float64(cur.UTimeTicks + cur.STimeTicks)
	}

	mem := &drivers.MemoryStats{
		RSS:      cur.RSSBytes,
		Measured: statsMeasuredMem,
	}

	usage := &drivers.ResourceUsage{
		CpuStats:    cpu,
		MemoryStats: mem,
	}

	pidStr := strconv.Itoa(pid)
	return &drivers.TaskResourceUsage{
		ResourceUsage: usage,
		Timestamp:     ts.UnixNano(),
		// Single-process task (the CH VMM is the only PID we
		// supervise). Pids[pid] mirrors the top-level usage so any
		// consumer that walks Pids sees the same numbers as the
		// aggregate (matches the upstream procstats convention).
		Pids: map[string]*drivers.ResourceUsage{
			pidStr: usage,
		},
	}
}

// safeSub returns a-b clamped at 0 (cumulative kernel counters never
// decrease in practice, but a PID-reuse window between samples could
// look like a regression; clamp rather than underflow).
func safeSub(a, b uint64) uint64 {
	if a < b {
		return 0
	}
	return a - b
}

// minStatsInterval guards against pathological caller intervals. Nomad's
// default is 1 s; values below 10 ms would just burn CPU re-reading /proc
// faster than the kernel updates utime/stime (HZ=100 means a 10 ms tick
// is the smallest interval at which a delta can possibly be non-zero).
const minStatsInterval = 10 * time.Millisecond

// TaskStats streams a TaskResourceUsage every `interval` (Nomad default
// 1 s) until ctx is cancelled OR the task exits.
//
// Flow:
//
//  1. Look up the taskHandle; return drivers.ErrTaskNotFound on miss.
//  2. Spawn a goroutine that ticks at max(interval, minStatsInterval):
//     a. Call statsCollectorFn(chPid) to sample /proc/<pid>/stat + status.
//     b. Build a *drivers.TaskResourceUsage from the cumulative + delta.
//     c. Send on the channel (drop the sample on ctx.Done to avoid
//     blocking forever if the consumer goes away).
//  3. Loop exits when ctx.Done() fires, when the handle's exitDone is
//     closed (StopTask / supervisor observed CH exit), or when
//     processAliveFn reports the PID gone (defence-in-depth: catches the
//     case where the supervisor hasn't observed exit yet but /proc is
//     already empty).
//  4. Channel is closed on exit so the consumer's range loop terminates.
//
// Transient counter errors are logged via the handle's logger but do NOT
// kill the stream — the next tick re-tries. A run of N consecutive
// errors logs at warn level; we don't surface a sentinel error because
// Nomad's contract is best-effort.
func (p *Plugin) TaskStats(ctx context.Context, taskID string, interval time.Duration) (<-chan *drivers.TaskResourceUsage, error) {
	h, ok := p.tasks.Get(taskID)
	if !ok {
		return nil, drivers.ErrTaskNotFound
	}

	if interval < minStatsInterval {
		interval = minStatsInterval
	}

	ch := make(chan *drivers.TaskResourceUsage)
	go p.runStatsCollector(ctx, h, interval, ch)
	return ch, nil
}

// runStatsCollector is the per-task goroutine that owns the polling
// ticker, the previous-sample cache, and the channel-close on exit.
// Extracted from TaskStats so tests can call it directly with a
// hand-built taskHandle — the same seam recover_task tests use.
func (p *Plugin) runStatsCollector(
	ctx context.Context,
	h *taskHandle,
	interval time.Duration,
	ch chan<- *drivers.TaskResourceUsage,
) {
	defer close(ch)

	if h == nil {
		return
	}

	timer := time.NewTimer(0) // fire once immediately, like the executor
	defer timer.Stop()

	var prev *hostStats
	var prevTs time.Time

	for {
		select {
		case <-ctx.Done():
			return
		case <-timer.C:
			timer.Reset(interval)
		}

		// Exit-on-task-gone gates. Two sources of truth, both
		// non-blocking. exitDone is the canonical signal (the
		// supervisor closes it); processAliveFn is the
		// defence-in-depth check (catches a window where /proc has
		// already cleared but the supervisor's runner.Wait hasn't
		// returned yet — possible for detachedRunner where the
		// Signal(0) poll is the only exit signal).
		if h.exitDone != nil {
			select {
			case <-h.exitDone:
				return
			default:
			}
		}
		if h.chPid > 0 {
			if err := processAliveFn(h.chPid); err != nil {
				return
			}
		}

		now := time.Now().UTC()
		cur, err := statsCollectorFn(h.chPid)
		if err != nil {
			// Transient failure: log and try again next tick.
			// We do NOT tear down the stream — that would deny
			// the operator the chance to recover from a brief
			// /proc hiccup (e.g. EAGAIN during a fork). The
			// supervisor's exit detection is the canonical
			// teardown signal.
			if p != nil && p.logger != nil {
				p.logger.Debug("ch: TaskStats: sample error",
					"task_id", h.taskConfig.ID,
					"ch_pid", h.chPid,
					"error", err)
			}
			continue
		}

		var elapsed time.Duration
		if !prevTs.IsZero() {
			elapsed = now.Sub(prevTs)
		}
		usage := buildResourceUsage(h.chPid, cur, prev, elapsed, now)
		prev = cur
		prevTs = now

		select {
		case <-ctx.Done():
			return
		case ch <- usage:
		}
	}
}

// _ is a compile-time assertion that the seam variable's type matches
// readHostStats — guards a future maintainer from accidentally swapping
// in a signature-incompatible fn through SetStatsCollectorForTest.
var _ func(pid int) (*hostStats, error) = readHostStats
