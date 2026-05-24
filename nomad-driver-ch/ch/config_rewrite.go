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
// C-7-LT-4 (2026-05-23): the bash wrapper's R15-S2 allow-list is
// ported here. After rewriting, every path-bearing field MUST resolve
// to a path under `taskDir` (the alloc's NOMAD_TASK_DIR). A snapshot
// whose `disks[].path` is something the rewriter could not anchor
// (e.g. an attacker-supplied `/etc/shadow`, or a path containing `..`
// components, or a relative path) is REJECTED with a clear error
// rather than silently passing through to CH. Mirrors
// `assert_under_task_dir` in nomad-vm-wrapper.sh:537-583.
//
// This was the C-7-LT-4 wedge: smoke-r15 caught CH's `--restore`
// aborting at +3ms with `CreateConsoleDevice(ENOENT)` because the
// driver bypasses the bash wrapper and was not enforcing the path
// rewrite at all. Pre-C-7-LT-4 the rewriter was permissive on
// "unmatched" paths (passed them through verbatim); post-C-7-LT-4 an
// unmatched path is a hard failure with a precise error pointing at
// both the offending field and the expected prefix.

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

// rewriteAndAssertUnderTaskDir is the rewrite-plus-allow-list pipeline
// for a single path-bearing field. It mirrors the bash wrapper's
// `rewrite(value)` followed by `assert_under_task_dir(field, value)`
// pair at nomad-vm-wrapper.sh:493-583.
//
// Algorithm:
//  1. If the input is the empty string, reject (an empty path is
//     never a thing CH should open; this defends against a hand-edited
//     config.json that wiped a path field).
//  2. Apply the alloc-prefix rewrite (rewriteSnapshotPath above). On a
//     match, the rewritten value is `taskDir + sep + remainder` and is
//     guaranteed to live under taskDir by construction.
//  3. R15-S2 allow-list — independently of (2), the post-rewrite path
//     MUST:
//     a. be absolute (start with `/`),
//     b. contain no `..` components,
//     c. live under `taskDir` (i.e. equal taskDir or have `taskDir/`
//        as its filepath.Clean'd prefix).
//
// We deliberately do NOT call os.Stat / filepath.EvalSymlinks here:
// the bash wrapper uses os.path.realpath which evaluates symlinks
// against the live filesystem, but Go's filesystem state at rewrite
// time may differ from CH's at open time (e.g. taskDir not yet
// populated). A symlink-followed check belongs to the operator's
// alloc-dir hygiene policy; the rewriter's job is to reject obvious
// out-of-tree paths and traversal attempts before they reach CH. If
// future hardening demands the realpath check, add it as a separate
// layer with an explicit os.Stat seam tests can swap.
//
// Returns the rewritten value on success, or an error naming the
// field, the offending value, and the expected prefix.
func rewriteAndAssertUnderTaskDir(fieldName, value, taskDir string) (string, error) {
	if value == "" {
		return "", fmt.Errorf("ch: rewriteConfigJSON: %s is empty; expected absolute path under %s", fieldName, taskDir)
	}
	rewritten := rewriteSnapshotPath(value, taskDir)

	if !strings.HasPrefix(rewritten, "/") {
		return "", fmt.Errorf("ch: rewriteConfigJSON: %s = %q is not absolute (expected a path under %s)", fieldName, value, taskDir)
	}
	// Path-component traversal defence: reject `..` as any component
	// of the post-rewrite value. The controller's rewriter never emits
	// one and CH never needs them; their presence is the red flag the
	// bash wrapper rejects too. Mirrors the explicit component scan
	// in assert_under_task_dir at nomad-vm-wrapper.sh:559-567.
	for _, part := range strings.Split(rewritten, "/") {
		if part == ".." {
			return "", fmt.Errorf("ch: rewriteConfigJSON: %s = %q contains a `..` component (path-traversal defence; expected a path under %s)", fieldName, value, taskDir)
		}
	}
	// Containment check. filepath.Clean collapses `.` segments and
	// repeated separators so a value like `<taskDir>/./x` and
	// `<taskDir>//x` both normalise to `<taskDir>/x`. We compare the
	// cleaned form so the prefix check is robust against benign-but-
	// surprising input shapes.
	cleaned := filepath.Clean(rewritten)
	cleanedTaskDir := filepath.Clean(taskDir)
	if cleaned != cleanedTaskDir && !strings.HasPrefix(cleaned, cleanedTaskDir+string(filepath.Separator)) {
		return "", fmt.Errorf("ch: rewriteConfigJSON: %s = %q resolves to %q, NOT under expected prefix %q (task_dir); possible malicious snapshot or misrouted restore", fieldName, value, cleaned, cleanedTaskDir)
	}
	return rewritten, nil
}

// rewriteConfigJSON path-rewrites the snapshot's config.json so disk
// paths + serial.file + console.file point at the NEW task dir, and
// the net[0].tap is rewritten to the tap derived from the NEW vm
// index (zsbx-nm-<idx>). MACs are NOT rewritten — the bash wrapper
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
//
// Output:
//   - rewritten bytes (json.Marshal output; compact, no trailing
//     newline) on success.
//   - Err on malformed input (non-JSON, unexpected types at known
//     keys).
//
// The rewrite is idempotent on re-wakes — a prior wake's task dir
// also matches the anchored prefix and is replaced with the current
// one.
//
// Post virtio-blk pivot the rewriter is expected to touch
// disks[].path and serial.file; older snapshots that carried
// fs[].socket entries will get those rewritten too for diagnostic
// clarity, but the restore would fail at CH level anyway (no
// virtiofsd backing the socket).
func rewriteConfigJSON(orig []byte, taskDir string, vmIndex uint16, subnetBaseOctet uint8) ([]byte, error) {
	if len(orig) == 0 {
		return nil, errors.New("ch: rewriteConfigJSON: empty input")
	}
	if taskDir == "" {
		return nil, errors.New("ch: rewriteConfigJSON: empty taskDir")
	}

	// We deliberately keep the document a generic map[string]any so
	// any extra CH fields we don't care about round-trip unchanged.
	// The chConfigDoc shape would be lossy on extras (CH tolerates
	// extra fields gracefully but rejects unknown ones at restore
	// time only if shape is wrong; the safest path is verbatim
	// preservation).
	var doc map[string]any
	if err := json.Unmarshal(orig, &doc); err != nil {
		return nil, fmt.Errorf("ch: rewriteConfigJSON: parse: %w", err)
	}

	// disks: []map[string]any. Each entry MAY have a "path" string.
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
					rew, err := rewriteAndAssertUnderTaskDir(fmt.Sprintf("disks[%d].path", i), ps, taskDir)
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

	// serial: map[string]any with optional "file" string.
	if rawSerial, ok := doc["serial"]; ok && rawSerial != nil {
		if sm, ok := rawSerial.(map[string]any); ok {
			if fv, ok := sm["file"]; ok {
				if fs, ok := fv.(string); ok {
					rew, err := rewriteAndAssertUnderTaskDir("serial.file", fs, taskDir)
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
					rew, err := rewriteAndAssertUnderTaskDir("console.file", fs, taskDir)
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
	// isn't backing the socket. The allow-list applies here too: a
	// snapshot whose fs[i].socket points outside the alloc dir is a
	// security red flag regardless of the legacy status.
	if rawFs, ok := doc["fs"]; ok && rawFs != nil {
		if fsEntries, ok := rawFs.([]any); ok {
			for i, e := range fsEntries {
				em, ok := e.(map[string]any)
				if !ok {
					continue
				}
				if sv, ok := em["socket"]; ok {
					if ss, ok := sv.(string); ok {
						rew, err := rewriteAndAssertUnderTaskDir(fmt.Sprintf("fs[%d].socket", i), ss, taskDir)
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
