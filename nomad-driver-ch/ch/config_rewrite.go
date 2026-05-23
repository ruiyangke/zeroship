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

package ch

import (
	"encoding/json"
	"errors"
	"fmt"
	"regexp"
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
					dm["path"] = rewriteSnapshotPath(ps, taskDir)
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
					sm["file"] = rewriteSnapshotPath(fs, taskDir)
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
					cm["file"] = rewriteSnapshotPath(fs, taskDir)
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
	// isn't backing the socket.
	if rawFs, ok := doc["fs"]; ok && rawFs != nil {
		if fsEntries, ok := rawFs.([]any); ok {
			for i, e := range fsEntries {
				em, ok := e.(map[string]any)
				if !ok {
					continue
				}
				if sv, ok := em["socket"]; ok {
					if ss, ok := sv.(string); ok {
						em["socket"] = rewriteSnapshotPath(ss, taskDir)
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
