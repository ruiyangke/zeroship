// Copyright (c) HashiCorp, Inc.
// SPDX-License-Identifier: MPL-2.0
//
// T-3 sprint: per-VM /30 tap network setup + teardown. Mirrors the
// host-side network plumbing in crates/sandbox/scripts/nomad-vm-wrapper.sh
// (cold-boot branch), lifted out of an external setup script and owned by
// the driver itself so cold-boot, restart, and (future) restore all funnel
// through the same code path.
//
// Subnet arithmetic (matches the bash wrapper line-for-line):
//
//	TAP       = zsbx-nm-${IDX}
//	SUBNET    = 10.${BASE}.${100+IDX}.0/30
//	HOST_IP   = 10.${BASE}.${100+IDX}.1
//	GUEST_IP  = 10.${BASE}.${100+IDX}.2   (advertised to the guest via cmdline)
//	MASK      = 255.255.255.252
//
// BASE defaults to 99 (preserving the historical 10.99/16 layout). IDX is
// the per-host VM index in [1,155]; the upper bound comes from the third
// octet (100+IDX) needing to fit a u8.
//
// Idempotency contract (so a Nomad-client restart that left a tap behind
// doesn't fail StartTask):
//
//   - `ip tuntap add` → if the device already exists, DELETE it first then
//     re-create. The vm_index is serialised by the controller, so any
//     pre-existing tap at the target name is from a prior alloc that
//     should have been torn down (either DestroyTask never ran, or its
//     best-effort teardown failed). Silently treating "already exists" as
//     success leaves the tap with whatever state the prior alloc imprinted
//     (DOWN/NO-CARRIER, possibly with the wrong IP); CH then emits the
//     "Tap %s already exists. IP configuration will not be overwritten."
//     WARN and exits. The pre-delete-on-collision policy ensures the next
//     `ip tuntap add` lands on a clean kernel state. Stderr shape on
//     collision: "ioctl(TUNSETIFF): Device or resource busy" (most kernels)
//     or "File exists".
//   - `ip addr add`   → if the address is already on the device, treat as
//     success. Stderr shape: "RTNETLINK answers: File exists".
//   - `ip link set up` → always idempotent (ip exits 0 if already up).
//   - `ip link delete` → if the device is already gone, treat as success.
//     Stderr shape: 'Cannot find device "<tap>"'.
//
// All real errors (EPERM, ENOSYS, malformed argv) bubble with the stderr
// captured so a Nomad task log shows "need root + CAP_NET_ADMIN" rather
// than an opaque exit code.

package ch

import (
	"errors"
	"fmt"
	"os/exec"
	"strings"
	"time"
)

// tapReleasePollAttempts caps the post-`ip link delete` poll loop on
// `ip link show <tap>` returning ENODEV. T-8b-stress-r3 saw two
// instances of `ioctl(TUNSETIFF): Device or resource busy` on the
// retry tuntap-add immediately following a collision-replace delete —
// the kernel's tun-driver doesn't release the netdev exclusive lock
// synchronously with `ip link delete`'s success return. Bounded poll
// avoids both pathological loops and an immediate-retry that hits
// EBUSY. 5 attempts × 100 ms = 500 ms wall worst case; in practice
// the kernel typically releases within one tick.
const tapReleasePollAttempts = 5

// tapReleasePollInterval is the per-attempt sleep in the post-delete
// poll loop. Picked at 100 ms so 5 attempts cap at 500 ms wall — well
// inside the 60 s alloc_running_timeout budget the controller waits
// for. Smaller intervals (e.g., 10 ms) would just spin syscalls
// without reducing real-world wait (kernel release is dominated by
// the tun-driver's internal cleanup tick, not poll cadence).
var tapReleasePollInterval = 100 * time.Millisecond

// defaultSubnetBaseOctet matches the bash wrapper's ZSBX_SUBNET_BASE_OCTET
// default (99). Exposed as a constant so tests pin the contract.
const defaultSubnetBaseOctet uint8 = 99

// defaultTapOwner is the user the tap is created under so the
// (downstream-spawned, unprivileged) cloud-hypervisor process can attach to
// it without CAP_NET_ADMIN. The bash wrapper relies on `tap_owner=nobody`
// being baked into the host fleet's pre-creation script; running the
// tuntap-add ourselves means we need to spell it out here.
const defaultTapOwner = "nobody"

// computeTapAddresses derives the per-VM /30 layout from (idx, base) and
// returns the human-readable strings consumed by `ip` invocations and by
// the kernel cmdline. Returns an error for idx=0 (the wrapper convention:
// vm_index is in [1,155], 0 is reserved / undefined).
//
// Pure function — no side effects, no I/O. Tests can pin the arithmetic
// without seam ceremony.
func computeTapAddresses(idx uint16, subnetBaseOctet uint8) (tapName, hostIP, guestIP, subnet string, err error) {
	if idx == 0 {
		return "", "", "", "", errors.New("ch: vm_index 0 is reserved (must be in [1,155])")
	}
	if idx > 155 {
		return "", "", "", "", fmt.Errorf("ch: vm_index %d out of range [1,155] (third octet 100+idx must fit u8)", idx)
	}
	base := subnetBaseOctet
	third := 100 + uint16(idx)
	tapName = fmt.Sprintf("zsbx-nm-%d", idx)
	hostIP = fmt.Sprintf("10.%d.%d.1", base, third)
	guestIP = fmt.Sprintf("10.%d.%d.2", base, third)
	subnet = fmt.Sprintf("10.%d.%d.0/30", base, third)
	return tapName, hostIP, guestIP, subnet, nil
}

// setupTapFn is the package-level seam tests swap out. Default
// (realSetupTap) shells to `ip` three times: tuntap-add, addr-add, link-up.
var setupTapFn = realSetupTap

// SetSetupTapForTest replaces the tap-setup seam so tests don't need
// CAP_NET_ADMIN. Returns the previous fn so the caller can restore it on
// cleanup.
func SetSetupTapForTest(fn func(idx uint16, subnetBaseOctet uint8) (string, error)) func(uint16, uint8) (string, error) {
	prev := setupTapFn
	if fn != nil {
		setupTapFn = fn
	}
	return prev
}

// setupTapForVM is the public entry point: routes through setupTapFn so
// tests can intercept. The default impl (realSetupTap) performs the
// three-step ip-command dance documented at the top of this file.
func setupTapForVM(idx uint16, subnetBaseOctet uint8) (string, error) {
	return setupTapFn(idx, subnetBaseOctet)
}

// realSetupTap performs the host-side /30 plumbing for one VM. Each
// step's idempotency policy is documented at the file-level comment.
//
// Steps mirror the bash wrapper's cold-boot tap setup, with the
// addition of a pre-delete on tap-add collision (T-8b-stress fix —
// see file-level comment):
//
//  1. ip tuntap add dev <tap> mode tap user <owner>
//     → on "already exists": ip link delete <tap> (tolerating
//       Cannot-find-device), then retry the tuntap-add ONCE.
//  2. ip addr add <host_ip>/30 dev <tap>
//  3. ip link set dev <tap> up
//
// Returns the tap name on success so the caller (StartTask) can record it
// on TaskState.Tap without re-deriving from idx.
func realSetupTap(idx uint16, subnetBaseOctet uint8) (string, error) {
	tapName, hostIP, _, _, err := computeTapAddresses(idx, subnetBaseOctet)
	if err != nil {
		return "", err
	}

	// Step 1: create the tap device. On collision (the prior alloc's
	// DestroyTask didn't tear it down — observed under T-8b-stress with
	// 9/11 wake failures), delete the stranded tap first then re-add.
	// vm_index serialisation makes the tap name driver-owned for the
	// lifetime of THIS alloc; leftover state is always safe to replace.
	if out, err := runIP("tuntap", "add", "dev", tapName, "mode", "tap", "user", defaultTapOwner); err != nil {
		if !isAlreadyExists(out) {
			return "", fmt.Errorf("ip tuntap add %s: %w (output=%q)", tapName, err, string(out))
		}
		// Tap leaked from a prior alloc. Delete it, then re-add. We
		// shell to `ip link delete` directly (not realTeardownTap) so
		// this step is isolated from the swappable teardownTapFn seam
		// — operators / tests pinning teardown don't accidentally
		// disable the collision-replace path. Tolerate
		// Cannot-find-device on the delete (race window: someone else
		// tore down the leftover between our tuntap-add EEXIST and
		// our delete).
		if delOut, delErr := runIP("link", "delete", tapName); delErr != nil {
			if !isNoSuchDevice(delOut) {
				return "", fmt.Errorf("ip link delete %s (collision-replace): %w (output=%q)", tapName, delErr, string(delOut))
			}
		}
		// T-8b-stress-r3 fix: poll for the kernel to release the
		// netdev before retrying tuntap-add. `ip link delete` returns
		// success synchronously but the tun-driver's exclusive lock on
		// the netdev is released asynchronously by the kernel — an
		// immediate retry hits `ioctl(TUNSETIFF): Device or resource
		// busy` (stress-r3: 2/60 CREATEs on worker-1). We poll
		// `ip link show <tap>` until it returns ENODEV ("Device
		// <tap> does not exist") with a bounded retry. Tolerate the
		// race where the kernel already released the netdev by the
		// time we poll (first attempt returns ENODEV) — that's the
		// happy path, just exits the loop immediately.
		if err := waitForTapAbsent(tapName); err != nil {
			return "", fmt.Errorf("ip link delete %s (collision-replace): kernel did not release netdev: %w", tapName, err)
		}
		// Second attempt at tuntap-add. If THIS still fails with
		// already-exists, something else races us (e.g., concurrent
		// driver / orchestration manipulating the same name) — surface
		// it loudly rather than masking.
		if out2, err2 := runIP("tuntap", "add", "dev", tapName, "mode", "tap", "user", defaultTapOwner); err2 != nil {
			return "", fmt.Errorf("ip tuntap add %s (after collision-replace): %w (output=%q)", tapName, err2, string(out2))
		}
	}

	// Step 2: assign the host-side /30 address.
	hostCIDR := hostIP + "/30"
	if out, err := runIP("addr", "add", hostCIDR, "dev", tapName); err != nil {
		if !isAlreadyExists(out) {
			return "", fmt.Errorf("ip addr add %s dev %s: %w (output=%q)", hostCIDR, tapName, err, string(out))
		}
		// Already assigned → idempotent success.
	}

	// Step 3: bring the link up. `ip link set up` returns 0 even on a
	// device that's already up, so we don't need an idempotency check
	// here.
	if out, err := runIP("link", "set", "dev", tapName, "up"); err != nil {
		return "", fmt.Errorf("ip link set %s up: %w (output=%q)", tapName, err, string(out))
	}

	return tapName, nil
}

// teardownTapFn is the package-level seam for `ip link delete <tap>`.
// Renamed from removeTapFn in T-2 → teardownTapFn in T-3 with idempotency
// fixed (Cannot-find-device tolerated). The old removeTapFn shim is kept
// below for back-compat with the T-2 stop_task call site.
var teardownTapFn = realTeardownTap

// SetTeardownTapForTest replaces the tap-teardown seam. Returns the
// previous fn so the caller can restore it on cleanup.
func SetTeardownTapForTest(fn func(tapName string) error) func(string) error {
	prev := teardownTapFn
	if fn != nil {
		teardownTapFn = fn
	}
	return prev
}

// teardownTap dispatches through the swappable seam.
func teardownTap(tapName string) error {
	return teardownTapFn(tapName)
}

// realTeardownTap is the production teardown: `ip link delete <tap>`.
// Tolerates "Cannot find device" — the post-crash residual case where the
// tap is already gone is the desired terminal state, so reporting it as an
// error would force the caller (DestroyTask) to discriminate the
// expected-missing case from real errors.
func realTeardownTap(tapName string) error {
	if tapName == "" {
		return errors.New("ch: teardownTap: empty tap name")
	}
	out, err := runIP("link", "delete", tapName)
	if err != nil {
		if isNoSuchDevice(out) {
			// Device already gone — idempotent success.
			return nil
		}
		return fmt.Errorf("ip link delete %s: %w (output=%q)", tapName, err, string(out))
	}
	return nil
}

// runIP is a thin wrapper around exec.Command("ip", ...) that returns both
// the combined stderr/stdout (for error-shape sniffing) and the exec error.
// Kept as a package-level var so a future test that needs to fake `ip`
// without going through setupTapFn/teardownTapFn can swap it directly.
var runIP = func(args ...string) ([]byte, error) {
	cmd := exec.Command("ip", args...)
	return cmd.CombinedOutput()
}

// sleepForTapPoll is the package-level seam tests swap so the poll
// loop in waitForTapAbsent doesn't add real wall time. Default is
// time.Sleep — production callers block on the kernel's tun release.
var sleepForTapPoll = func(d time.Duration) {
	time.Sleep(d)
}

// waitForTapAbsent polls `ip link show <tap>` until the kernel reports
// ENODEV (no such device), or `tapReleasePollAttempts` ticks pass.
// Used after `ip link delete <tap>` in the collision-replace path so a
// subsequent `ip tuntap add` doesn't race the tun-driver's asynchronous
// netdev release (stress-r3 mechanism for `ioctl(TUNSETIFF): Device or
// resource busy` on the retry tuntap-add).
//
// Returns nil as soon as `ip link show` returns ENODEV. Returns an
// error if all attempts exhaust without seeing ENODEV — that means the
// netdev is wedged in the tun-driver's queue (rare but bounded
// reporting beats an opaque EBUSY downstream).
//
// Tolerates the "device already gone" race on the FIRST poll
// (kernel released synchronously by the time we got here) — that's
// the happy path.
func waitForTapAbsent(tapName string) error {
	for i := 0; i < tapReleasePollAttempts; i++ {
		out, err := runIP("link", "show", tapName)
		if err != nil && isNoSuchDevice(out) {
			// ENODEV — kernel released the netdev. Safe to retry
			// tuntap-add.
			return nil
		}
		// Either `ip link show` succeeded (tap still present) or it
		// failed with a non-ENODEV stderr (transient netlink error,
		// privilege issue). Retry until budget exhausts; if the
		// surface is a real error (EPERM etc.), the subsequent
		// tuntap-add will surface it loudly with the EBUSY/EEXIST
		// shape.
		if i+1 < tapReleasePollAttempts {
			sleepForTapPoll(tapReleasePollInterval)
		}
	}
	return fmt.Errorf("tap %s still present after %d × %v poll", tapName, tapReleasePollAttempts, tapReleasePollInterval)
}

// isAlreadyExists matches the stderr shapes `ip` emits when the device or
// address we're adding is already present. Case-insensitive substring
// match: keeps the matcher resilient to minor wording drift between
// iproute2 versions.
//
// Observed shapes:
//   - "ioctl(TUNSETIFF): Device or resource busy"
//   - "RTNETLINK answers: File exists"
//   - "File exists"
//   - "Address already assigned"
func isAlreadyExists(out []byte) bool {
	s := strings.ToLower(string(out))
	switch {
	case strings.Contains(s, "file exists"):
		return true
	case strings.Contains(s, "device or resource busy"):
		return true
	case strings.Contains(s, "already assigned"):
		return true
	}
	return false
}

// isNoSuchDevice matches the stderr shapes `ip` emits when the link we're
// addressing is gone. Two distinct shapes depending on the subcommand:
//
//   - `ip link delete <tap>` (when the device is already gone):
//     'Cannot find device "<tap>"'
//   - `ip link show <tap>`   (when the device doesn't exist):
//     'Device "<tap>" does not exist.'
//
// Both shapes mean ENODEV. The collision-replace post-delete poll
// (T-8b-stress-r3) drives `ip link show` so this matcher must cover
// both. Case-insensitive substring keeps the matcher resilient to
// minor wording drift across iproute2 versions.
func isNoSuchDevice(out []byte) bool {
	s := strings.ToLower(string(out))
	switch {
	case strings.Contains(s, "cannot find device"):
		return true
	case strings.Contains(s, "does not exist"):
		// "Device \"X\" does not exist." — `ip link show` shape.
		return true
	}
	return false
}
