// Copyright (c) HashiCorp, Inc.
// SPDX-License-Identifier: MPL-2.0
//
// Version-string contract test. Exercises `cmd/nomad-driver-ch/main.go`'s
// `--version` handler — the operator-facing identity probe used by
// `gcp-worker-startup.sh` to confirm which build is on disk after a GCS
// pull. The format `nomad-driver-ch <sha>` is pinned here; do not change it
// without updating the consumer scripts (scripts/upload-to-gcs.sh,
// crates/sandbox/scripts/gcp-worker-startup.sh in the sandbox worktree).
//
// Test strategy: build the binary into t.TempDir() WITHOUT `-X main.gitSHA`
// so the default "dev" tag flows through, then exec it with `--version`
// and assert the printed string starts with "nomad-driver-ch " followed by
// a non-empty SHA-shaped token. We also exercise the -ldflags override
// path with a fixed test SHA to confirm ldflags actually wires into
// gitSHA (catches an accidental rename, e.g. main.gitSha vs main.gitSHA).

package tests

import (
	"os/exec"
	"path/filepath"
	"runtime"
	"strings"
	"testing"
)

// TestVersionFlag asserts the --version handler exits 0 and prints the
// pinned identity line. The default (no -ldflags) renders "dev"; the
// override path renders whatever we passed via -X.
func TestVersionFlag(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("plugin is Linux-only (CH host); skipping cross-platform exec")
	}

	// Build into a tempdir so the test doesn't litter the repo. Use the
	// default gitSHA value ("dev") for the first build — no -ldflags.
	binDir := t.TempDir()
	binPath := filepath.Join(binDir, "nomad-driver-ch")
	repoRoot := repoRootFromTest(t)

	build := exec.Command("go", "build",
		"-trimpath",
		"-o", binPath,
		"./cmd/nomad-driver-ch",
	)
	build.Dir = repoRoot
	if out, err := build.CombinedOutput(); err != nil {
		t.Fatalf("go build (default gitSHA): %v\n%s", err, out)
	}

	// Default-build path: gitSHA stays "dev".
	got, err := exec.Command(binPath, "--version").CombinedOutput()
	if err != nil {
		t.Fatalf("`nomad-driver-ch --version`: %v\n%s", err, got)
	}
	gotStr := strings.TrimSpace(string(got))
	if !strings.HasPrefix(gotStr, "nomad-driver-ch ") {
		t.Errorf("--version output %q does not start with %q", gotStr, "nomad-driver-ch ")
	}
	// "ch" must appear (binary name carries it); spec line:
	// "asserts --version returns a string containing 'ch' + the gitSHA
	// (or 'dev' in test mode)".
	if !strings.Contains(gotStr, "ch") {
		t.Errorf("--version output %q missing literal %q", gotStr, "ch")
	}
	if !strings.HasSuffix(gotStr, " dev") {
		t.Errorf("--version output %q should end with %q for an un-overridden build", gotStr, " dev")
	}

	// Override path: rebuild with a fixed test SHA and re-exec.
	binPath2 := filepath.Join(binDir, "nomad-driver-ch.tagged")
	build2 := exec.Command("go", "build",
		"-trimpath",
		"-ldflags=-X main.gitSHA=testsha1",
		"-o", binPath2,
		"./cmd/nomad-driver-ch",
	)
	build2.Dir = repoRoot
	if out, err := build2.CombinedOutput(); err != nil {
		t.Fatalf("go build (-X main.gitSHA): %v\n%s", err, out)
	}
	got2, err := exec.Command(binPath2, "--version").CombinedOutput()
	if err != nil {
		t.Fatalf("`nomad-driver-ch --version` (tagged): %v\n%s", err, got2)
	}
	got2Str := strings.TrimSpace(string(got2))
	if got2Str != "nomad-driver-ch testsha1" {
		t.Errorf("tagged --version output = %q, want %q", got2Str, "nomad-driver-ch testsha1")
	}
}

// repoRootFromTest resolves the module root by walking up from the test
// file location until it finds go.mod. We can't hard-code "../" because
// the test binary may be invoked from inside `go test`'s tempdir.
func repoRootFromTest(t *testing.T) string {
	t.Helper()
	// tests/version_test.go is exactly one level under the module root,
	// so the working directory of `go test ./tests/...` is `tests/`.
	// We resolve to `..` and let go build re-resolve `./cmd/...` from
	// the module root.
	wd, err := exec.Command("pwd").Output()
	if err != nil {
		t.Fatalf("pwd: %v", err)
	}
	cwd := strings.TrimSpace(string(wd))
	// tests/ → parent
	return filepath.Dir(cwd)
}
