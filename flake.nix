{
  description = "zeroship - AI-native app platform";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
      in
      {
        devShells.default = pkgs.mkShell {
          buildInputs = with pkgs; [
            nodejs_22
            nodePackages.npm
            bun
            rustc
            cargo
            # clippy from the same nixpkgs as rustc/cargo above, so its
            # bundled clippy-driver matches the toolchain version (a
            # mismatched clippy fails with E0514 against crates already
            # built by this rustc).
            clippy
            # rustfmt from the same nixpkgs as rustc/cargo, for the same reason
            # clippy is pinned above. Without it `cargo fmt` is simply absent
            # from the shell -- `cargo fmt --all -- --check` failed with "no
            # such command" every time it was asked for, which reads like a
            # passing check if the exit code is not inspected.
            rustfmt
            cmake
            pkg-config
            openssl
            curl.dev
            # SQLite CLI for ad-hoc inspection of dev/test databases.
            # Note: rusqlite uses the `bundled` Cargo feature in
            # crates/plugin-db, so it does NOT link against this sqlite —
            # it compiles the SQLite 3.51.x amalgamation into our binary.
            # Keep the CLI in the shell only for `sqlite3 <file>` debugging.
            sqlite
            # `pg_dump` and `pg_restore` for the data suite's snapshot tests.
            # `xtask test data` refuses to run unless both are on PATH at the
            # same major version as the fixture server in
            # tests/fixtures/postgres/Dockerfile; without them the suite stops
            # at its preflight instead of reporting on the code. Bump this
            # attribute with that Dockerfile's tag.
            postgresql_16
            # libclang + clang are needed by rusqlite's `preupdate_hook`
            # Cargo feature, which uses `bindgen` to generate Rust
            # bindings against the bundled SQLite headers (see
            # crates/plugin-db/Cargo.toml [sqlite] feature). LIBCLANG_PATH
            # + BINDGEN_EXTRA_CLANG_ARGS below tell bindgen where to find
            # the runtime + system headers.
            llvmPackages.libclang
            llvmPackages.clang
            wrk
            numactl
            hey
            esbuild
            # Profiling
            linuxPackages.perf
            cargo-flamegraph
            # Playwright — `playwright` CLI for e2e tests without
            # polluting node_modules. Browsers come from
            # PLAYWRIGHT_BROWSERS_PATH below; the version of the CLI
            # must match the bundled chromium build (currently 1208).
            playwright-test

            # --- CI parity ------------------------------------------------
            # CI runs inside this shell (`.github/workflows/ci.yml` sets
            # `shell: nix develop --command bash -e {0}`). These are the tools
            # the harnesses and CI steps invoke that the flake did not already
            # supply. Without them the workflow needs a second, separately
            # maintained apt list, and those two definitions drift.
            #
            # cargo-nextest replaces taiki-e/install-action. lsof and zstd are
            # harness dependencies: golden_path.sh frees its ports with
            # `lsof -ti` and reads the artifact manifest with `tar --zstd`.
            # git, procps, which, netcat, net-tools and jq are invoked by
            # tests/provision_test_backends.sh, tests/deploy_scripts_gate.sh
            # and the e2e harnesses.
            #
            # pnpm is deliberately absent. package.json#packageManager pins the
            # pnpm CI must use, and a devShell entry is PREPENDED to PATH, so a
            # shell-provided pnpm would shadow that pin (this nixpkgs carries an
            # older major than the pin). Move it here only with a nixpkgs bump.
            cargo-nextest
            git
            lsof
            zstd
            jq
            procps
            which
            netcat
            net-tools
          ];

          RUST_BACKTRACE = "1";
          # bindgen (used by rusqlite preupdate_hook + other -sys crates)
          # needs to locate libclang.so + clang's system headers at
          # build time. Without these, `cargo build --features sqlite`
          # fails with "Unable to find libclang".
          LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
          # Two -isystem entries:
          #   1. clang's own resource headers (stddef.h, stdarg.h, …) —
          #      required by every bindgen consumer, incl. rusqlite.
          #   2. glibc's dev headers (sys/types.h, …) — required by
          #      `pg_query`/libpg_query (crates/zeroship-migrate), whose
          #      generated `pg_query.h` pulls in `<sys/types.h>`. rusqlite's
          #      bundled amalgamation never needed (2), so it was absent;
          #      pg_query's bindgen fails with "'sys/types.h' file not found"
          #      without it. `stdenv.cc.libc.dev` is the same glibc the
          #      toolchain links against (no version skew).
          BINDGEN_EXTRA_CLANG_ARGS =
            "-isystem ${pkgs.llvmPackages.libclang.lib}/lib/clang/${pkgs.lib.getVersion pkgs.llvmPackages.clang}/include "
            + "-isystem ${pkgs.stdenv.cc.libc.dev}/include";
          # Playwright: use Nix-provided browsers, npm provides the test runner
          PLAYWRIGHT_BROWSERS_PATH = "${pkgs.playwright-driver.browsers}";
          PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD = "1";
        };
      });
}
