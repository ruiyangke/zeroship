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
            pkg-config
            openssl
            # SQLite CLI for ad-hoc inspection of dev/test databases.
            # Note: rusqlite uses the `bundled` Cargo feature in
            # crates/plugin-db, so it does NOT link against this sqlite —
            # it compiles the SQLite 3.51.x amalgamation into our binary.
            # Keep the CLI in the shell only for `sqlite3 <file>` debugging.
            sqlite
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
          ];

          RUST_BACKTRACE = "1";
          # bindgen (used by rusqlite preupdate_hook + other -sys crates)
          # needs to locate libclang.so + clang's system headers at
          # build time. Without these, `cargo build --features sqlite`
          # fails with "Unable to find libclang".
          LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
          BINDGEN_EXTRA_CLANG_ARGS =
            "-isystem ${pkgs.llvmPackages.libclang.lib}/lib/clang/${pkgs.lib.getVersion pkgs.llvmPackages.clang}/include";
          # Playwright: use Nix-provided browsers, npm provides the test runner
          PLAYWRIGHT_BROWSERS_PATH = "${pkgs.playwright-driver.browsers}";
          PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD = "1";
        };
      });
}
