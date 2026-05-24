// Copyright (c) HashiCorp, Inc.
// SPDX-License-Identifier: MPL-2.0
//
// T-3 sprint test surface: pins the per-VM /30 subnet arithmetic and the
// idempotency contract of the setup / teardown plumbing in ch/net.go.
//
// The "real" setup/teardown paths shell to `ip`; tests swap that seam
// (ch.SetRunIPForTest) to inject fake stderr shapes — so we exercise the
// EEXIST / Cannot-find-device branches without needing CAP_NET_ADMIN or
// touching the host's network namespace.

package tests

import (
	"errors"
	"fmt"
	"os/exec"
	"strings"
	"sync"
	"sync/atomic"
	"testing"

	"github.com/zeroship/nomad-driver-ch/ch"
)

// --- arithmetic ---------------------------------------------------------

// TestComputeTapAddresses_HappyPath pins the /30 layout for a representative
// (idx, base) pair. Mirrors the bash wrapper's
// `TAP=zsbx-nm-${IDX}; SUBNET=10.${BASE}.${100+IDX}.0/30`.
func TestComputeTapAddresses_HappyPath(t *testing.T) {
	tap, host, guest, subnet, err := ch.ComputeTapAddresses(99, 99)
	if err != nil {
		t.Fatalf("ComputeTapAddresses: %v", err)
	}
	if tap != "zsbx-nm-99" {
		t.Errorf("tapName = %q, want zsbx-nm-99", tap)
	}
	// idx=99, base=99 → third octet = 100+99 = 199
	if host != "10.99.199.1" {
		t.Errorf("hostIP = %q, want 10.99.199.1", host)
	}
	if guest != "10.99.199.2" {
		t.Errorf("guestIP = %q, want 10.99.199.2", guest)
	}
	if subnet != "10.99.199.0/30" {
		t.Errorf("subnet = %q, want 10.99.199.0/30", subnet)
	}
}

// TestComputeTapAddresses_BoundaryIndex covers the wrapper's documented
// [1,155] range (the upper bound comes from 100+idx needing to fit a u8).
// Third octet at idx=1 → 101, at idx=155 → 255.
func TestComputeTapAddresses_BoundaryIndex(t *testing.T) {
	cases := []struct {
		idx                                  uint16
		base                                 uint8
		wantTap, wantHost, wantGuest, wantNw string
	}{
		{1, 99, "zsbx-nm-1", "10.99.101.1", "10.99.101.2", "10.99.101.0/30"},
		{155, 99, "zsbx-nm-155", "10.99.255.1", "10.99.255.2", "10.99.255.0/30"},
		// A non-default base octet to cover the BASE knob's plumbing.
		{42, 200, "zsbx-nm-42", "10.200.142.1", "10.200.142.2", "10.200.142.0/30"},
	}
	for _, tc := range cases {
		t.Run(fmt.Sprintf("idx=%d_base=%d", tc.idx, tc.base), func(t *testing.T) {
			tap, host, guest, subnet, err := ch.ComputeTapAddresses(tc.idx, tc.base)
			if err != nil {
				t.Fatalf("ComputeTapAddresses(%d,%d): %v", tc.idx, tc.base, err)
			}
			if tap != tc.wantTap {
				t.Errorf("tapName = %q, want %q", tap, tc.wantTap)
			}
			if host != tc.wantHost {
				t.Errorf("hostIP = %q, want %q", host, tc.wantHost)
			}
			if guest != tc.wantGuest {
				t.Errorf("guestIP = %q, want %q", guest, tc.wantGuest)
			}
			if subnet != tc.wantNw {
				t.Errorf("subnet = %q, want %q", subnet, tc.wantNw)
			}
		})
	}
}

// TestComputeTapAddresses_RejectsZero ensures idx=0 is refused (the bash
// wrapper's convention: vm_index 0 is reserved). idx>155 is also refused
// because the third octet (100+idx) would overflow u8.
func TestComputeTapAddresses_RejectsZero(t *testing.T) {
	if _, _, _, _, err := ch.ComputeTapAddresses(0, 99); err == nil {
		t.Error("expected error for idx=0, got nil")
	}
	if _, _, _, _, err := ch.ComputeTapAddresses(156, 99); err == nil {
		t.Error("expected error for idx=156 (overflow), got nil")
	}
	if _, _, _, _, err := ch.ComputeTapAddresses(255, 99); err == nil {
		t.Error("expected error for idx=255 (overflow), got nil")
	}
}

// --- setupTap idempotency ----------------------------------------------

// ipRecorder captures every `ip ...` invocation the seam'd runner sees,
// and decides what to return per-call via a programmable script. Thread-
// safe so a parallel test can't tear it.
type ipRecorder struct {
	mu      sync.Mutex
	calls   [][]string
	scripts []ipScript
	cursor  int
}

type ipScript struct {
	// matchArgv[0] is matched against argv[0] (e.g. "tuntap", "addr", "link")
	// only; nil = match-any. Keeps the script terse.
	matchPrefix string
	// out is what CombinedOutput would emit (stderr from `ip` lands here).
	out []byte
	// err is the exec.Cmd error to surface. Use nil for success.
	err error
}

func newIPRecorder(scripts ...ipScript) *ipRecorder {
	return &ipRecorder{scripts: scripts}
}

func (r *ipRecorder) run(args ...string) ([]byte, error) {
	r.mu.Lock()
	defer r.mu.Unlock()
	// Capture the call.
	cp := make([]string, len(args))
	copy(cp, args)
	r.calls = append(r.calls, cp)

	// Find the next matching script. If matchPrefix is empty, match.
	// Otherwise, argv[0] must equal matchPrefix.
	for r.cursor < len(r.scripts) {
		s := r.scripts[r.cursor]
		r.cursor++
		if s.matchPrefix == "" || (len(args) > 0 && args[0] == s.matchPrefix) {
			return s.out, s.err
		}
	}
	// Default: success with empty output. Tests that exercise the
	// happy path don't need to script every step.
	return nil, nil
}

func (r *ipRecorder) recorded() [][]string {
	r.mu.Lock()
	defer r.mu.Unlock()
	cp := make([][]string, len(r.calls))
	for i, c := range r.calls {
		cc := make([]string, len(c))
		copy(cc, c)
		cp[i] = cc
	}
	return cp
}

// fakeExitErr makes our scripted `ip` "exit" non-zero. The error type
// doesn't matter — only `err != nil` does, plus the stderr/stdout bytes
// the matchers inspect.
type fakeExitErr struct{ msg string }

func (e *fakeExitErr) Error() string { return e.msg }

// argvEqual returns true iff `argv` matches `want` in full.
func argvEqual(argv, want []string) bool {
	if len(argv) != len(want) {
		return false
	}
	for i := range argv {
		if argv[i] != want[i] {
			return false
		}
	}
	return true
}

// TestSetupTap_HappyPath asserts the three `ip` commands the setup path
// issues, in order, with the right arguments.
func TestSetupTap_HappyPath(t *testing.T) {
	rec := newIPRecorder() // empty script: every call defaults to success
	prev := ch.SetRunIPForTest(rec.run)
	t.Cleanup(func() { ch.SetRunIPForTest(prev) })

	tap, err := ch.CallRealSetupTap(7, 99)
	if err != nil {
		t.Fatalf("realSetupTap: %v", err)
	}
	if tap != "zsbx-nm-7" {
		t.Errorf("tap = %q, want zsbx-nm-7", tap)
	}

	calls := rec.recorded()
	if len(calls) != 3 {
		t.Fatalf("call count = %d, want 3 (tuntap-add, addr-add, link-set-up): %v", len(calls), calls)
	}

	// Step 1: ip tuntap add dev zsbx-nm-7 mode tap user nobody
	wantStep1 := []string{"tuntap", "add", "dev", "zsbx-nm-7", "mode", "tap", "user", "nobody"}
	if !argvEqual(calls[0], wantStep1) {
		t.Errorf("step 1: %v, want %v", calls[0], wantStep1)
	}

	// Step 2: ip addr add 10.99.107.1/30 dev zsbx-nm-7
	wantStep2 := []string{"addr", "add", "10.99.107.1/30", "dev", "zsbx-nm-7"}
	if !argvEqual(calls[1], wantStep2) {
		t.Errorf("step 2: %v, want %v", calls[1], wantStep2)
	}

	// Step 3: ip link set dev zsbx-nm-7 up
	wantStep3 := []string{"link", "set", "dev", "zsbx-nm-7", "up"}
	if !argvEqual(calls[2], wantStep3) {
		t.Errorf("step 3: %v, want %v", calls[2], wantStep3)
	}
}

// TestSetupTap_DeletesAndReAddsOnExistingTap simulates iproute2 emitting
// "Device or resource busy" on tuntap-add — the device was left behind by
// a prior alloc whose DestroyTask never ran (or whose best-effort teardown
// failed). The driver MUST delete the stranded tap first, then re-add it,
// rather than silently treating "already exists" as success (which leaves
// the kernel state from the prior alloc and surfaces downstream as CH's
// "Tap %s already exists. IP configuration will not be overwritten." WARN
// followed by exit -1).
//
// Closes T-8b-stress Bug 2: 9/11 wake failures on tap-already-exists.
// vm_index serialisation makes the tap name driver-owned, so a leftover
// at the target name is always safe to replace.
func TestSetupTap_DeletesAndReAddsOnExistingTap(t *testing.T) {
	rec := newIPRecorder(
		// Call 1: tuntap-add → EEXIST (the prior alloc's leftover).
		ipScript{
			matchPrefix: "tuntap",
			out:         []byte("ioctl(TUNSETIFF): Device or resource busy\n"),
			err:         &fakeExitErr{msg: "exit status 2"},
		},
		// Call 2: link delete → success (defaults handle this; but pin
		// explicitly so a future re-ordering surfaces here, not in a
		// flaky cluster smoke).
		ipScript{matchPrefix: "link", out: nil, err: nil},
		// Call 3: tuntap-add retry → success (defaults: nil/nil).
		// Call 4: addr-add → success.
		// Call 5: link-set-up → success.
	)
	prev := ch.SetRunIPForTest(rec.run)
	t.Cleanup(func() { ch.SetRunIPForTest(prev) })

	if _, err := ch.CallRealSetupTap(7, 99); err != nil {
		t.Fatalf("realSetupTap should pre-delete + re-add on existing tap, got %v", err)
	}
	calls := rec.recorded()
	if len(calls) != 5 {
		t.Fatalf("call count = %d, want 5 (tuntap-add-eexist, link-del, tuntap-add-retry, addr-add, link-set-up): %v", len(calls), calls)
	}

	// Pin the EXACT sequence — operator hand-debug depends on this
	// ordering when triaging stranded-tap leftovers.
	wantSeq := [][]string{
		{"tuntap", "add", "dev", "zsbx-nm-7", "mode", "tap", "user", "nobody"},
		{"link", "delete", "zsbx-nm-7"},
		{"tuntap", "add", "dev", "zsbx-nm-7", "mode", "tap", "user", "nobody"},
		{"addr", "add", "10.99.107.1/30", "dev", "zsbx-nm-7"},
		{"link", "set", "dev", "zsbx-nm-7", "up"},
	}
	for i, want := range wantSeq {
		if !argvEqual(calls[i], want) {
			t.Errorf("call %d: %v, want %v", i, calls[i], want)
		}
	}
}

// TestSetupTap_ToleratesDeleteRaceOnReAdd simulates the delete step
// returning Cannot-find-device — the leftover tap was torn down between
// our tuntap-add EEXIST and our delete (e.g., another process raced us
// to teardown). The retry tuntap-add must still succeed.
func TestSetupTap_ToleratesDeleteRaceOnReAdd(t *testing.T) {
	rec := newIPRecorder(
		ipScript{
			matchPrefix: "tuntap",
			out:         []byte("ioctl(TUNSETIFF): Device or resource busy\n"),
			err:         &fakeExitErr{msg: "exit status 2"},
		},
		// link delete races with another teardown → Cannot-find-device.
		ipScript{
			matchPrefix: "link",
			out:         []byte("Cannot find device \"zsbx-nm-7\"\n"),
			err:         &fakeExitErr{msg: "exit status 1"},
		},
		// retry tuntap-add succeeds; addr/link defaults.
	)
	prev := ch.SetRunIPForTest(rec.run)
	t.Cleanup(func() { ch.SetRunIPForTest(prev) })

	if _, err := ch.CallRealSetupTap(7, 99); err != nil {
		t.Fatalf("realSetupTap should tolerate Cannot-find-device on delete-race, got %v", err)
	}
	calls := rec.recorded()
	if len(calls) != 5 {
		t.Errorf("call count = %d, want 5: %v", len(calls), calls)
	}
}

// TestSetupTap_SurfacesUnrecognizedDeleteFailure asserts that a real
// failure on the collision-replace delete (e.g., EPERM, EBUSY on a
// kernel that wedges the device) bubbles up as a clean error rather than
// being silently absorbed. Catches the inverse risk of the previous
// "silently treat EEXIST as success" bug.
func TestSetupTap_SurfacesUnrecognizedDeleteFailure(t *testing.T) {
	rec := newIPRecorder(
		ipScript{
			matchPrefix: "tuntap",
			out:         []byte("ioctl(TUNSETIFF): Device or resource busy\n"),
			err:         &fakeExitErr{msg: "exit status 2"},
		},
		ipScript{
			matchPrefix: "link",
			out:         []byte("RTNETLINK answers: Operation not permitted\n"),
			err:         &fakeExitErr{msg: "exit status 2"},
		},
	)
	prev := ch.SetRunIPForTest(rec.run)
	t.Cleanup(func() { ch.SetRunIPForTest(prev) })

	_, err := ch.CallRealSetupTap(7, 99)
	if err == nil {
		t.Fatal("expected error on unrecognised delete failure, got nil")
	}
	if !strings.Contains(err.Error(), "collision-replace") {
		t.Errorf("err = %v, want hint that this is the collision-replace path", err)
	}
	if !strings.Contains(err.Error(), "Operation not permitted") {
		t.Errorf("err = %v, want stderr captured", err)
	}
}

// TestSetupTap_SurfacesPersistentEexist asserts that if the retry
// tuntap-add ALSO returns EEXIST (some external process actively
// re-creating the tap concurrently — pathological / non-vm_index-owned
// scenario), we DON'T loop forever; the second EEXIST surfaces as an
// error so the operator can investigate.
func TestSetupTap_SurfacesPersistentEexist(t *testing.T) {
	rec := newIPRecorder(
		ipScript{
			matchPrefix: "tuntap",
			out:         []byte("ioctl(TUNSETIFF): Device or resource busy\n"),
			err:         &fakeExitErr{msg: "exit status 2"},
		},
		// delete → success (defaults)
		ipScript{matchPrefix: "link", out: nil, err: nil},
		// retry tuntap-add ALSO EEXIST — surface it.
		ipScript{
			matchPrefix: "tuntap",
			out:         []byte("File exists\n"),
			err:         &fakeExitErr{msg: "exit status 2"},
		},
	)
	prev := ch.SetRunIPForTest(rec.run)
	t.Cleanup(func() { ch.SetRunIPForTest(prev) })

	_, err := ch.CallRealSetupTap(7, 99)
	if err == nil {
		t.Fatal("expected error on persistent EEXIST after collision-replace, got nil")
	}
	if !strings.Contains(err.Error(), "after collision-replace") {
		t.Errorf("err = %v, want hint that the retry path failed", err)
	}
}

// TestSetupTap_IdempotentOnExistingAddr simulates `ip addr add` returning
// "File exists" (the address was already on the device). Setup must still
// succeed.
func TestSetupTap_IdempotentOnExistingAddr(t *testing.T) {
	rec := newIPRecorder(
		ipScript{matchPrefix: "tuntap"}, // success
		ipScript{
			matchPrefix: "addr",
			out:         []byte("RTNETLINK answers: File exists\n"),
			err:         &fakeExitErr{msg: "exit status 2"},
		},
		// link defaults to success
	)
	prev := ch.SetRunIPForTest(rec.run)
	t.Cleanup(func() { ch.SetRunIPForTest(prev) })

	if _, err := ch.CallRealSetupTap(7, 99); err != nil {
		t.Fatalf("realSetupTap should tolerate existing addr, got %v", err)
	}
	calls := rec.recorded()
	if len(calls) != 3 {
		t.Errorf("call count = %d, want 3: %v", len(calls), calls)
	}
}

// TestSetupTap_BubblesUnexpectedErr asserts that an EPERM-shaped error
// (unrelated to the idempotency cases) bubbles with the stderr captured
// so the operator sees the actionable hint.
func TestSetupTap_BubblesUnexpectedErr(t *testing.T) {
	rec := newIPRecorder(
		ipScript{
			matchPrefix: "tuntap",
			out:         []byte("ip: RTNETLINK answers: Operation not permitted\n"),
			err:         &fakeExitErr{msg: "exit status 2"},
		},
	)
	prev := ch.SetRunIPForTest(rec.run)
	t.Cleanup(func() { ch.SetRunIPForTest(prev) })

	_, err := ch.CallRealSetupTap(7, 99)
	if err == nil {
		t.Fatal("expected error for EPERM, got nil")
	}
	if !strings.Contains(err.Error(), "Operation not permitted") {
		t.Errorf("err = %v, want stderr captured (contains 'Operation not permitted')", err)
	}
	if !strings.Contains(err.Error(), "tuntap") && !strings.Contains(err.Error(), "zsbx-nm-7") {
		t.Errorf("err = %v, want hint about which step / tap failed", err)
	}
}

// --- teardownTap idempotency -------------------------------------------

// TestTeardownTap_HappyPath asserts the teardown path issues exactly one
// `ip link delete <tap>` and returns nil on success.
func TestTeardownTap_HappyPath(t *testing.T) {
	rec := newIPRecorder() // empty script: defaults to success
	prev := ch.SetRunIPForTest(rec.run)
	t.Cleanup(func() { ch.SetRunIPForTest(prev) })

	if err := ch.CallRealTeardownTap("zsbx-nm-7"); err != nil {
		t.Fatalf("realTeardownTap: %v", err)
	}
	calls := rec.recorded()
	if len(calls) != 1 {
		t.Fatalf("call count = %d, want 1", len(calls))
	}
	want := []string{"link", "delete", "zsbx-nm-7"}
	if !argvEqual(calls[0], want) {
		t.Errorf("argv = %v, want %v", calls[0], want)
	}
}

// TestTeardownTap_TolerantOfMissing simulates the post-crash residual case:
// `ip link delete` returns "Cannot find device" because the tap is already
// gone. Teardown must return nil (the goal is "tap is gone" — already-gone
// is success).
func TestTeardownTap_TolerantOfMissing(t *testing.T) {
	rec := newIPRecorder(
		ipScript{
			matchPrefix: "link",
			out:         []byte("Cannot find device \"zsbx-nm-7\"\n"),
			err:         &fakeExitErr{msg: "exit status 1"},
		},
	)
	prev := ch.SetRunIPForTest(rec.run)
	t.Cleanup(func() { ch.SetRunIPForTest(prev) })

	if err := ch.CallRealTeardownTap("zsbx-nm-7"); err != nil {
		t.Errorf("teardown should tolerate Cannot-find-device, got %v", err)
	}
}

// TestTeardownTap_BubblesUnexpectedErr — an unrecognised stderr surfaces
// as an error. Defence-in-depth so a future iproute2 wording change
// doesn't silently turn real failures into nil returns.
func TestTeardownTap_BubblesUnexpectedErr(t *testing.T) {
	rec := newIPRecorder(
		ipScript{
			matchPrefix: "link",
			out:         []byte("ip: RTNETLINK answers: Operation not permitted\n"),
			err:         &fakeExitErr{msg: "exit status 2"},
		},
	)
	prev := ch.SetRunIPForTest(rec.run)
	t.Cleanup(func() { ch.SetRunIPForTest(prev) })

	err := ch.CallRealTeardownTap("zsbx-nm-7")
	if err == nil {
		t.Fatal("expected error for EPERM, got nil")
	}
	if !strings.Contains(err.Error(), "Operation not permitted") {
		t.Errorf("err = %v, want stderr captured", err)
	}
}

// --- StartTask integration ---------------------------------------------

// TestStartTask_InvokesSetupTap extends the T-1 scaffold: with no Net
// entry on TaskConfig, StartTask must call setupTapForVM with the
// (vm_index, subnet_base_octet) derived from task_config.
func TestStartTask_InvokesSetupTap(t *testing.T) {
	chBin := writeStubBinary(t, "cloud-hypervisor")
	t.Setenv("ZSBX_CH_BIN", chBin)

	// Capture the setup call args. Returns the tap name realSetupTap
	// would have returned so StartTask can record it on TaskState.
	var (
		mu          sync.Mutex
		gotIdx      uint16
		gotBase     uint8
		callCount   atomic.Int32
		returnedTap string
	)
	prevSetup := ch.SetSetupTapForTest(func(idx uint16, base uint8) (string, error) {
		mu.Lock()
		gotIdx = idx
		gotBase = base
		mu.Unlock()
		callCount.Add(1)
		returnedTap = fmt.Sprintf("zsbx-nm-%d", idx)
		return returnedTap, nil
	})
	t.Cleanup(func() { ch.SetSetupTapForTest(prevSetup) })

	// Even with the setupTapFn seam swapped, ensureTapUpFn is still on the
	// operator-supplied-Net branch — clear it for this run.
	prevTapUp := ch.SetEnsureTapUpForTest(func(string) error { return nil })
	t.Cleanup(func() { ch.SetEnsureTapUpForTest(prevTapUp) })

	cfg := validColdBootConfig()
	cfg.VMIndex = 42
	cfg.SubnetBaseOctet = 99
	// CRUCIAL: do NOT set Net. That's what routes StartTask through
	// setupTapForVM rather than the legacy operator-pinned tap-up path.

	taskDir := t.TempDir()
	p, taskCfg := newTestPluginWithFactory(t, &cfg, taskDir, func(cmd *exec.Cmd) ch.ProcessRunnerSeam {
		return newFakeRunner(cmd)
	})

	handle, _, err := p.StartTask(taskCfg)
	if err != nil {
		t.Fatalf("StartTask: %v", err)
	}
	if handle == nil {
		t.Fatal("nil handle")
	}

	if got := callCount.Load(); got != 1 {
		t.Errorf("setupTap call count = %d, want 1", got)
	}
	mu.Lock()
	defer mu.Unlock()
	if gotIdx != 42 {
		t.Errorf("setupTap idx = %d, want 42", gotIdx)
	}
	if gotBase != 99 {
		t.Errorf("setupTap base = %d, want 99", gotBase)
	}

	// The persisted TaskState should carry the tap the seam returned —
	// proving StartTask uses resolveNet's name (which already mirrors
	// the bash wrapper) and is consistent with what setupTapForVM
	// allocated.
	var state ch.TaskState
	if err := handle.GetDriverState(&state); err != nil {
		t.Fatalf("GetDriverState: %v", err)
	}
	if state.Tap != returnedTap {
		t.Errorf("TaskState.Tap = %q, want %q", state.Tap, returnedTap)
	}
}

// TestStartTask_SetupTap_DefaultsSubnetBaseOctet — when TaskConfig leaves
// SubnetBaseOctet unset (0), StartTask must fall back to the bash
// wrapper's default of 99. Pinned here because the default lives in
// driver code, not the operator's HCL.
func TestStartTask_SetupTap_DefaultsSubnetBaseOctet(t *testing.T) {
	chBin := writeStubBinary(t, "cloud-hypervisor")
	t.Setenv("ZSBX_CH_BIN", chBin)

	var gotBase uint8
	prevSetup := ch.SetSetupTapForTest(func(idx uint16, base uint8) (string, error) {
		gotBase = base
		return fmt.Sprintf("zsbx-nm-%d", idx), nil
	})
	t.Cleanup(func() { ch.SetSetupTapForTest(prevSetup) })
	prevTapUp := ch.SetEnsureTapUpForTest(func(string) error { return nil })
	t.Cleanup(func() { ch.SetEnsureTapUpForTest(prevTapUp) })

	cfg := validColdBootConfig()
	cfg.SubnetBaseOctet = 0 // unset → driver should default to 99

	p, taskCfg := newTestPluginWithFactory(t, &cfg, t.TempDir(), func(cmd *exec.Cmd) ch.ProcessRunnerSeam {
		return newFakeRunner(cmd)
	})
	if _, _, err := p.StartTask(taskCfg); err != nil {
		t.Fatalf("StartTask: %v", err)
	}
	if gotBase != 99 {
		t.Errorf("default SubnetBaseOctet = %d, want 99", gotBase)
	}
}

// TestStartTask_SetupTap_SurfacesError — when setupTapFn returns an error
// (e.g. EPERM in production), StartTask must fail with a message that
// includes the vm_index so the operator can correlate against the task.
func TestStartTask_SetupTap_SurfacesError(t *testing.T) {
	chBin := writeStubBinary(t, "cloud-hypervisor")
	t.Setenv("ZSBX_CH_BIN", chBin)

	sentinel := errors.New("synthetic EPERM (need CAP_NET_ADMIN)")
	prevSetup := ch.SetSetupTapForTest(func(uint16, uint8) (string, error) {
		return "", sentinel
	})
	t.Cleanup(func() { ch.SetSetupTapForTest(prevSetup) })

	cfg := validColdBootConfig()
	cfg.VMIndex = 7

	p, taskCfg := newTestPluginWithFactory(t, &cfg, t.TempDir(), func(cmd *exec.Cmd) ch.ProcessRunnerSeam {
		return newFakeRunner(cmd)
	})
	_, _, err := p.StartTask(taskCfg)
	if err == nil {
		t.Fatal("expected error when setupTap fails, got nil")
	}
	if !errors.Is(err, sentinel) {
		t.Errorf("err = %v, want wrapping of sentinel", err)
	}
	if !strings.Contains(err.Error(), "vm_index=7") {
		t.Errorf("err = %v, want vm_index=7 in message", err)
	}
}
