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
//   - `ip tuntap add` → if the device already exists, treat the error as
//     success. Stderr shape: "ioctl(TUNSETIFF): Device or resource busy"
//     (most kernels) or "File exists".
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
)

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

// realSetupTap performs the host-side /30 plumbing for one VM. Idempotent
// on each step; see the file-level comment for the precise error
// semantics.
//
// Steps mirror the bash wrapper's cold-boot tap setup:
//  1. ip tuntap add dev <tap> mode tap user <owner>
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

	// Step 1: create the tap device.
	if out, err := runIP("tuntap", "add", "dev", tapName, "mode", "tap", "user", defaultTapOwner); err != nil {
		if !isAlreadyExists(out) {
			return "", fmt.Errorf("ip tuntap add %s: %w (output=%q)", tapName, err, string(out))
		}
		// Already exists → idempotent success.
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

// isNoSuchDevice matches the stderr shape `ip` emits when the link we're
// addressing is gone.
//
// Observed shape:
//   - 'Cannot find device "<tap>"'
func isNoSuchDevice(out []byte) bool {
	s := strings.ToLower(string(out))
	return strings.Contains(s, "cannot find device")
}
