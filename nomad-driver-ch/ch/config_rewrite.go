// Copyright (c) HashiCorp, Inc.
// SPDX-License-Identifier: MPL-2.0
//
// T-6 sprint: rewrite the snapshot's config.json so the disk paths +
// serial.file path + net tap name target THIS alloc's NOMAD_TASK_DIR
// and THIS host's tap, rather than the source alloc's.
//
// This is the Go port of the Python rewriter embedded in
// crates/sandbox-snapshot-restore/crates/sandbox/scripts/
// nomad-vm-wrapper.sh lines ~430-555. The bash version's anchored
// prefix matcher is replicated here:
//
//     ^/opt/nomad/data/alloc/<non-slash>+/<non-slash>+/local(/|$)
//
// so a path like `/opt/nomad/data/alloc/AAA/bbb/local/workspace.img`
// rewrites to `<task_dir>/workspace.img`, while `…/local-fake` (no
// path-separator boundary) is left alone.
//
// W1 (security-r8): the rewrite operates only on JSON *string* values
// at known keys (disks[].path, serial.file, console.file, net[].tap)
// — non-string values are passed through verbatim, so a hand-edited
// config.json that put e.g. a number where CH expects a path cannot
// be silently mis-substituted.
//
// C-7-LT-4 (2026-05-23): the bash wrapper's R15-S2 allow-list was
// ported here. After rewriting, every path-bearing field MUST resolve
// to a path under `taskDir`.
//
// C-7-LT-6 (2026-05-23): the single "under task_dir" invariant was
// TOO STRICT for `disks[*].path`. The per-sandbox persistent workspace
// lives at `/var/zeroship/ch/<sandbox_id>/workspace.img` — OUTSIDE the
// alloc task_dir by design (must persist across alloc lifecycle /
// restore). C-7-LT-4 rejected it as `possible malicious snapshot or
// misrouted restore`, blocking every real wake-from-snapshot at the
// driver before CH ever spawned (smoke-r16).
//
// C-7-LT-7 (2026-05-23): C-7-LT-6 was correct but ONE ENTRY SHORT.
// The per-user persistent home image lives at
// `/var/zeroship/ch/users/<user_id>/home.img` — OUTSIDE the per-
// sandbox prefix by design (one home shared across many sandboxes
// owned by the same user; package caches and dotfiles persist).
// Smoke-r17 surfaced this as `disks[2].path = ".../users/<usr>/home.img"
// NOT under any allow-list prefix`; the rewriter failed-fast on
// `disks[1]` (workspace) in r16, then on `disks[2]` (home) in r17.
//
// Fix shape: a per-field allow-list keyed by the JSON field's *kind*:
//
//   - PathFieldRuntimeFile (serial.file, console.file): runtime files
//     CH writes inside the alloc dir. MUST be under task_dir.
//   - PathFieldDisk (disks[*].path): persistent block devices. MAY be
//     under the per-sandbox prefix `/var/zeroship/ch/<sandbox_id>/`,
//     OR under the per-user home prefix `/var/zeroship/ch/users/<user_id>/`
//     (C-7-LT-7), OR under task_dir (e.g. a freshly-staged rootfs.img),
//     OR under any caller-supplied content-addressed root (read-only
//     base images shared across sandboxes).
//   - PathFieldFsSocket (fs[*].socket): legacy virtio-fs sockets,
//     scoped to the alloc dir like runtime files.
//
// Rewriting semantics:
//   - The alloc-prefix rewriter (rewriteSnapshotPath) still runs on
//     every path-bearing field. For disks this means: a disk that
//     pointed at the OLD alloc's task_dir gets rewritten to the NEW
//     alloc's task_dir; a disk that pointed at `/var/zeroship/ch/...`
//     is left alone (the per-sandbox path is stable across alloc
//     lifecycle and must NOT be rewritten — it survives the alloc).
//
// Cross-check with the bash wrapper (nomad-vm-wrapper.sh:476-624):
// the wrapper's `assert_under_task_dir` enforces strict task_dir
// containment for ALL fields including disks[*].path. The wrapper
// would also fail on the persistent-workspace path; in production the
// wrapper relies on the rootfs being copied INTO the alloc dir by
// other steps (nomad-vm-wrapper.sh:300-305) and the workspace+userhome
// paths being supplied via env vars resolved BEFORE the rewriter
// runs. The Go driver bypasses the wrapper and consumes the
// snapshot's config.json *as the snapshot recorded it* — which is
// where the persistent workspace path leaks through. The per-field
// allow-list is the contract the wrapper imposed implicitly via its
// layout; we make it explicit in the Go port.
//
// Defence-in-depth properties retained from C-7-LT-4 / extended in
// C-7-LT-7:
//   - Per-tenant isolation: a snapshot that names another sandbox's
//     prefix (`/var/zeroship/ch/sbx_OTHER/...`) is REJECTED. The
//     per-sandbox prefix is the CURRENT alloc's sandbox_id, not a
//     wildcard.
//   - Per-user isolation (C-7-LT-7): a snapshot that names another
//     user's home prefix (`/var/zeroship/ch/users/usr_OTHER/...`) is
//     REJECTED. The per-user prefix uses the CURRENT alloc's user_id,
//     not a wildcard. Empty user_id disables the per-user slot
//     entirely (mismatch is rejected, NOT silently accepted).
//   - `..` traversal rejected as ANY path component.
//   - Empty / non-absolute paths rejected.
//   - Random absolute paths (`/etc/shadow`, `/run/attacker.sock`)
//     rejected — they're under none of the allow-list roots.
//
// Future-direction note (per smoke-r17 review): the recommended
// long-term shape is to enumerate the snapshot's `disks[]` and accept
// each path AFTER validating its prefix against a known zeroship-
// managed sub-namespace (approach A). This file currently implements
// the simpler per-namespace allow-list (approach B): three layout-
// stable prefixes (sandbox + user_home + task_dir + optional content-
// addressed roots). If a fourth legitimate namespace surfaces,
// migrate to approach A rather than chaining C-7-LT-8/9/10.

package ch

import (
	"encoding/json"
	"errors"
	"fmt"
	"path/filepath"
	"regexp"
	"strings"
)

// allocPrefixRe matches the anchored Nomad per-task local-dir layout
// the snapshot's paths use. group(1) is the boundary character
// (either "/" or "") which the rewriter preserves so `…/local`
// rewrites to `<task_dir>` (no trailing slash) and `…/local/x`
// rewrites to `<task_dir>/x`.
//
// The trailing group enforces a path-separator boundary so accidental
// prefixes like `/opt/nomad/data/alloc/x/y/localfoo` are NOT matched.
var allocPrefixRe = regexp.MustCompile(`^/opt/nomad/data/alloc/[^/]+/[^/]+/local(/|$)`)

// PathFieldKind classifies which allow-list applies to a given
// config.json field.
type PathFieldKind int

const (
	// PathFieldRuntimeFile covers serial.file + console.file: CH
	// runtime files written inside the alloc dir. Validated under
	// task_dir only.
	PathFieldRuntimeFile PathFieldKind = iota
	// PathFieldDisk covers disks[*].path: persistent block devices.
	// Validated under per-sandbox prefix OR task_dir OR any
	// content-addressed root.
	PathFieldDisk
	// PathFieldFsSocket covers fs[*].socket: legacy virtio-fs
	// sockets, alloc-scoped like runtime files.
	PathFieldFsSocket
)

// sandboxPrefix returns the per-sandbox persistent root for the given
// sandbox id. Empty sandboxID returns "" — callers MUST treat that as
// "no per-sandbox prefix available" (the validator falls back to
// task_dir + content-addressed roots only).
//
// Layout: `/var/zeroship/ch/<sandbox_id>/` — matches the layout the
// controller emits and the bash wrapper consumes
// (crates/sandbox/src/backend/nomad_ch.rs § ZSBX_RESTORE_FROM, and
// crates/sandbox/scripts/nomad-vm-wrapper.sh § ZSBX_WORKSPACE_IMG).
// The trailing separator is included so the prefix check rejects
// `/var/zeroship/ch/sbx_xyz_other/...` (a different sandbox whose id
// happens to share a textual prefix).
func sandboxPrefix(sandboxID string) string {
	if sandboxID == "" {
		return ""
	}
	return filepath.Join("/var/zeroship/ch", sandboxID) + string(filepath.Separator)
}

// userHomePrefix returns the per-user persistent home root for the
// given user id. Empty userID returns "" — callers MUST treat that as
// "no per-user prefix available" (the validator falls back to per-
// sandbox + task_dir + content-addressed roots only).
//
// Layout: `/var/zeroship/ch/users/<user_id>/` — matches the layout
// the controller's `user_home_image_path` emits
// (crates/sandbox/src/backend/nomad_ch.rs § user_home_image_path).
// The trailing separator is included so the prefix check rejects
// `/var/zeroship/ch/users/usr_xyz_other/...` (a different user whose
// id happens to share a textual prefix). C-7-LT-7.
func userHomePrefix(userID string) string {
	if userID == "" {
		return ""
	}
	return filepath.Join("/var/zeroship/ch/users", userID) + string(filepath.Separator)
}

// rewriteSnapshotPath returns a rewritten string if value matches the
// anchored alloc-prefix pattern, else returns value unchanged.
//
// Pure function — no I/O, no allocation beyond the string concat.
func rewriteSnapshotPath(value, taskDir string) string {
	loc := allocPrefixRe.FindStringSubmatchIndex(value)
	if loc == nil {
		return value
	}
	// loc[0]..loc[1] is the full match. loc[2]..loc[3] is group(1)
	// — the boundary character (either "/" or empty, the latter
	// indicating EOS).
	sep := ""
	if loc[3] > loc[2] {
		sep = value[loc[2]:loc[3]]
	}
	remainder := value[loc[1]:]
	return taskDir + sep + remainder
}

// hasPathPrefix reports whether `path` equals `prefix` (cleaned) or
// has `prefix + separator` as its cleaned prefix. The cleaning step
// collapses `.` segments and repeated separators so a value like
// `<prefix>/./x` and `<prefix>//x` both normalise to `<prefix>/x`.
//
// Callers MUST ensure `..` components have been rejected upstream —
// hasPathPrefix is a string-prefix check, not a realpath check.
//
// Returns false if either arg is empty.
func hasPathPrefix(path, prefix string) bool {
	if path == "" || prefix == "" {
		return false
	}
	cleanedPath := filepath.Clean(path)
	cleanedPrefix := filepath.Clean(prefix)
	if cleanedPath == cleanedPrefix {
		return true
	}
	return strings.HasPrefix(cleanedPath, cleanedPrefix+string(filepath.Separator))
}

// validatePathByKind enforces the allow-list appropriate to `kind` on
// the already-rewritten `value`. Returns nil on success, or a
// descriptive error naming both the offending value and the expected
// prefix(es).
//
// Common pre-checks (applied for every kind):
//   - empty path → reject
//   - relative path → reject (CH only accepts absolute paths)
//   - any `..` component → reject (path-traversal defence in depth)
//
// Kind-specific checks:
//   - PathFieldRuntimeFile / PathFieldFsSocket: MUST be under
//     task_dir. Mirrors C-7-LT-4 verbatim for these kinds.
//   - PathFieldDisk: MUST be under the per-sandbox prefix
//     (/var/zeroship/ch/<sandbox_id>/), OR under the per-user home
//     prefix (/var/zeroship/ch/users/<user_id>/, C-7-LT-7), OR under
//     task_dir, OR under any caller-supplied content-addressed root.
//
// Per-tenant isolation note: the per-sandbox prefix uses the CURRENT
// alloc's sandbox_id and the per-user prefix uses the CURRENT alloc's
// user_id. A snapshot that names another sandbox/user's prefix fails
// this check — it's neither under THIS sandbox's prefix nor THIS
// user's home nor task_dir nor a content-addressed root, so it falls
// through to the rejection branch.
func validatePathByKind(
	kind PathFieldKind,
	fieldName, origValue, value, taskDir, sandboxID, userID string,
	contentAddressedRoots []string,
) error {
	if value == "" {
		return fmt.Errorf("ch: rewriteConfigJSON: %s is empty; expected absolute path under %s", fieldName, taskDir)
	}
	if !strings.HasPrefix(value, "/") {
		return fmt.Errorf("ch: rewriteConfigJSON: %s = %q is not absolute (expected a path under %s)", fieldName, origValue, taskDir)
	}
	// Path-component traversal defence: reject `..` as ANY component
	// of the post-rewrite value. The controller's rewriter never emits
	// one and CH never needs them; their presence is the red flag the
	// bash wrapper rejects too. Mirrors the explicit component scan in
	// assert_under_task_dir at nomad-vm-wrapper.sh:559-567.
	for _, part := range strings.Split(value, "/") {
		if part == ".." {
			return fmt.Errorf("ch: rewriteConfigJSON: %s = %q contains a `..` component (path-traversal defence; expected a path under %s)", fieldName, origValue, taskDir)
		}
	}

	cleaned := filepath.Clean(value)

	switch kind {
	case PathFieldRuntimeFile, PathFieldFsSocket:
		if !hasPathPrefix(cleaned, taskDir) {
			return fmt.Errorf("ch: rewriteConfigJSON: %s = %q resolves to %q, NOT under expected prefix %q (task_dir); possible malicious snapshot or misrouted restore", fieldName, origValue, cleaned, filepath.Clean(taskDir))
		}
		return nil
	case PathFieldDisk:
		// Allow-list, in order:
		//   1. per-sandbox persistent prefix (the smoke-r16 case)
		//   2. per-user home prefix (C-7-LT-7, the smoke-r17 case)
		//   3. task_dir (staged disks like rootfs.img the driver
		//      materialises at cold-boot, which a re-snap captures)
		//   4. caller-supplied content-addressed roots (read-only
		//      base images shared across sandboxes)
		if sandboxID != "" {
			if hasPathPrefix(cleaned, sandboxPrefix(sandboxID)) {
				return nil
			}
		}
		if userID != "" {
			if hasPathPrefix(cleaned, userHomePrefix(userID)) {
				return nil
			}
		}
		if hasPathPrefix(cleaned, taskDir) {
			return nil
		}
		for _, root := range contentAddressedRoots {
			if root == "" {
				continue
			}
			if hasPathPrefix(cleaned, root) {
				return nil
			}
		}
		// Build a descriptive error naming the prefixes we DID check
		// so an operator looking at the Nomad event can tell at a
		// glance which allow-list slot the path missed.
		var allowed []string
		if sandboxID != "" {
			allowed = append(allowed, fmt.Sprintf("sandbox=%s prefix %q", sandboxID, sandboxPrefix(sandboxID)))
		}
		if userID != "" {
			allowed = append(allowed, fmt.Sprintf("user=%s home prefix %q", userID, userHomePrefix(userID)))
		}
		allowed = append(allowed, fmt.Sprintf("task_dir %q", filepath.Clean(taskDir)))
		for _, root := range contentAddressedRoots {
			if root == "" {
				continue
			}
			allowed = append(allowed, fmt.Sprintf("content-addressed root %q", filepath.Clean(root)))
		}
		return fmt.Errorf("ch: rewriteConfigJSON: %s = %q resolves to %q, NOT under any allow-list prefix [%s]; possible malicious snapshot or misrouted restore", fieldName, origValue, cleaned, strings.Join(allowed, "; "))
	default:
		return fmt.Errorf("ch: rewriteConfigJSON: %s: unknown PathFieldKind %d (internal bug)", fieldName, kind)
	}
}

// rewriteAndValidatePath is the per-field pipeline: apply the
// alloc-prefix rewrite (which is unconditional — it's a no-op on
// already-correct paths and a substitution on old-alloc paths) and
// then validate by kind.
//
// Per-sandbox `/var/zeroship/ch/<sbx>/...` paths do NOT match the
// alloc-prefix regex (which anchors at `/opt/nomad/data/alloc/...`)
// so they pass through rewriteSnapshotPath unchanged — preserving the
// "stable across alloc lifecycle" property the smoke-r16 review
// called out.
//
// Returns the (possibly rewritten) value on success, or an error.
func rewriteAndValidatePath(
	kind PathFieldKind,
	fieldName, value, taskDir, sandboxID, userID string,
	contentAddressedRoots []string,
) (string, error) {
	if value == "" {
		return "", fmt.Errorf("ch: rewriteConfigJSON: %s is empty; expected absolute path under %s", fieldName, taskDir)
	}
	rewritten := rewriteSnapshotPath(value, taskDir)
	if err := validatePathByKind(kind, fieldName, value, rewritten, taskDir, sandboxID, userID, contentAddressedRoots); err != nil {
		return "", err
	}
	return rewritten, nil
}

// rewriteConfigJSON path-rewrites the snapshot's config.json so disk
// paths + serial.file + console.file point at the NEW task dir (or
// preserve their persistent per-sandbox location), and the
// net[0].tap is rewritten to the tap derived from the NEW vm index
// (zsbx-nm-<idx>). MACs are NOT rewritten — the bash wrapper
// preserves them too; a stable MAC across snapshot/restore lets the
// guest's eth0 keep its existing ARP cache.
//
// Inputs:
//   - orig: the snapshot's config.json bytes verbatim (CH-shaped JSON).
//   - taskDir: NOMAD_TASK_DIR for the new alloc (absolute path).
//   - vmIndex: the new alloc's per-host VM index.
//   - subnetBaseOctet: the new alloc's second-octet (used only for
//     diagnostic clarity in the future; today the bash wrapper does
//     not rewrite IPs because the snapshot's recorded kernel cmdline
//     is replayed verbatim and CH `--restore` ignores --cmdline).
//   - sandboxID: the current alloc's sandbox id (typed_id form, no
//     prefix). Empty disables the per-sandbox allow-list entry
//     (disks must then be under task_dir or a content-addressed
//     root). C-7-LT-6: required for legitimate persistent
//     workspace.img paths.
//   - userID: the current alloc's user id (typed_id form, no
//     prefix). Empty disables the per-user-home allow-list entry
//     (disks must then be under per-sandbox/task_dir/content-
//     addressed roots only). C-7-LT-7: required for legitimate
//     per-user home.img paths under `/var/zeroship/ch/users/<id>/`.
//   - contentAddressedRoots: absolute paths under which read-only
//     base images live (e.g. `/var/zeroship/ch/rootfs/`). May be
//     nil/empty — that simply disables the content-addressed allow-
//     list slot.
//
// Output:
//   - rewritten bytes (json.Marshal output; compact, no trailing
//     newline) on success.
//   - Err on malformed input (non-JSON, unexpected types at known
//     keys) OR on any path that fails the per-field allow-list.
func rewriteConfigJSON(
	orig []byte,
	taskDir string,
	vmIndex uint16,
	subnetBaseOctet uint8,
	sandboxID, userID string,
	contentAddressedRoots []string,
) ([]byte, error) {
	if len(orig) == 0 {
		return nil, errors.New("ch: rewriteConfigJSON: empty input")
	}
	if taskDir == "" {
		return nil, errors.New("ch: rewriteConfigJSON: empty taskDir")
	}

	// We deliberately keep the document a generic map[string]any so
	// any extra CH fields we don't care about round-trip unchanged.
	var doc map[string]any
	if err := json.Unmarshal(orig, &doc); err != nil {
		return nil, fmt.Errorf("ch: rewriteConfigJSON: parse: %w", err)
	}

	// disks: []map[string]any. Each entry MAY have a "path" string.
	// C-7-LT-6 + C-7-LT-7: validated as PathFieldDisk (allow-list:
	// per-sandbox prefix OR per-user home prefix OR task_dir OR
	// content-addressed root).
	if rawDisks, ok := doc["disks"]; ok && rawDisks != nil {
		disks, ok := rawDisks.([]any)
		if !ok {
			return nil, fmt.Errorf("ch: rewriteConfigJSON: 'disks' is %T, want []any", rawDisks)
		}
		for i, d := range disks {
			dm, ok := d.(map[string]any)
			if !ok {
				continue
			}
			if pv, ok := dm["path"]; ok {
				if ps, ok := pv.(string); ok {
					rew, err := rewriteAndValidatePath(
						PathFieldDisk,
						fmt.Sprintf("disks[%d].path", i),
						ps, taskDir, sandboxID, userID, contentAddressedRoots,
					)
					if err != nil {
						return nil, err
					}
					dm["path"] = rew
				}
			}
			disks[i] = dm
		}
		doc["disks"] = disks
	}

	// serial: map[string]any with optional "file" string. Always
	// under task_dir (CH writes the in-guest console log there).
	if rawSerial, ok := doc["serial"]; ok && rawSerial != nil {
		if sm, ok := rawSerial.(map[string]any); ok {
			if fv, ok := sm["file"]; ok {
				if fs, ok := fv.(string); ok {
					rew, err := rewriteAndValidatePath(
						PathFieldRuntimeFile,
						"serial.file",
						fs, taskDir, sandboxID, userID, contentAddressedRoots,
					)
					if err != nil {
						return nil, err
					}
					sm["file"] = rew
				}
			}
			doc["serial"] = sm
		}
	}

	// console: same shape as serial.
	if rawConsole, ok := doc["console"]; ok && rawConsole != nil {
		if cm, ok := rawConsole.(map[string]any); ok {
			if fv, ok := cm["file"]; ok {
				if fs, ok := fv.(string); ok {
					rew, err := rewriteAndValidatePath(
						PathFieldRuntimeFile,
						"console.file",
						fs, taskDir, sandboxID, userID, contentAddressedRoots,
					)
					if err != nil {
						return nil, err
					}
					cm["file"] = rew
				}
			}
			doc["console"] = cm
		}
	}

	// net: []map[string]any. Each entry MAY have a "tap" string.
	// Unlike paths, tap names don't share an anchored prefix — we
	// rewrite unconditionally to the new alloc's tap derived from
	// vmIndex. MAC stays stable (preserves guest ARP).
	if rawNet, ok := doc["net"]; ok && rawNet != nil {
		nets, ok := rawNet.([]any)
		if !ok {
			return nil, fmt.Errorf("ch: rewriteConfigJSON: 'net' is %T, want []any", rawNet)
		}
		newTap := fmt.Sprintf("zsbx-nm-%d", vmIndex)
		for i, n := range nets {
			nm, ok := n.(map[string]any)
			if !ok {
				continue
			}
			if _, has := nm["tap"]; has {
				nm["tap"] = newTap
			}
			nets[i] = nm
		}
		doc["net"] = nets
	}

	// fs[].socket — legacy virtio-fs sockets. Rewritten for diagnostic
	// clarity; restore will still fail at CH level when virtiofsd
	// isn't backing the socket. Validated under task_dir (the only
	// place a virtio-fs socket would legitimately live in a CH
	// alloc).
	if rawFs, ok := doc["fs"]; ok && rawFs != nil {
		if fsEntries, ok := rawFs.([]any); ok {
			for i, e := range fsEntries {
				em, ok := e.(map[string]any)
				if !ok {
					continue
				}
				if sv, ok := em["socket"]; ok {
					if ss, ok := sv.(string); ok {
						rew, err := rewriteAndValidatePath(
							PathFieldFsSocket,
							fmt.Sprintf("fs[%d].socket", i),
							ss, taskDir, sandboxID, userID, contentAddressedRoots,
						)
						if err != nil {
							return nil, err
						}
						em["socket"] = rew
					}
				}
				fsEntries[i] = em
			}
			doc["fs"] = fsEntries
		}
	}

	// subnetBaseOctet is accepted for forward-compat (future cmdline
	// rewrites) but not consumed today; reference it so the
	// signature-vs-go-vet contract holds.
	_ = subnetBaseOctet

	out, err := json.Marshal(doc)
	if err != nil {
		return nil, fmt.Errorf("ch: rewriteConfigJSON: marshal: %w", err)
	}
	return out, nil
}
