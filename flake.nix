{
  description = "zeroship - AI-native app platform";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
    flake-utils.url = "github:numtide/flake-utils";
    # Web Platform Tests, pinned to one upstream commit. The runtime's `wpt`
    # test target `include_str!`s a WPT checkout under
    # `crates/zeroship-runtime/tests/wpt`, which is gitignored and has no
    # tracked file. The development shell below symlinks this input into place,
    # so a fresh checkout compiles the target without a fetch step. Bump the rev
    # after the `wpt_*.rs` runners are verified green against a newer tree, then
    # run `nix flake lock`.
    wpt = {
      url = "github:web-platform-tests/wpt/e053afbbd005bed4b6100f98f0de744da8d1d09d";
      flake = false;
    };
  };

  outputs = { self, nixpkgs, flake-utils, wpt }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        inherit (pkgs) lib stdenv;

        # bindgen (libsqlite3-sys, pg_query, v8) drives libclang directly, so
        # the cc wrapper's flags never reach it: name the system headers here.
        # Darwin names them as a sysroot, since libSystem carries no headers.
        bindgenClangArgs =
          lib.optionals stdenv.hostPlatform.isLinux [ "-isystem ${stdenv.cc.libc.dev}/include" ]
          ++ lib.optionals stdenv.hostPlatform.isDarwin [ "-isysroot ${pkgs.apple-sdk.sdkroot}" ];

        # The workspace Playwright catalog tracks this driver's version; see
        # xtask/tests/playwright_browsers.rs.
        playwright-driver = pkgs.playwright-driver;

        # `xtask test <area>` from anywhere in the repository, with no setup
        # beyond the shell and no knowledge of where the checkout is. The
        # `cargo xtask` alias stays; this is the same invocation with the root
        # resolved at run time.
        xtask = pkgs.writeShellScriptBin "xtask" ''
          set -euo pipefail
          root="$(git rev-parse --show-toplevel)"
          exec cargo run --manifest-path "$root/xtask/Cargo.toml" -- "$@"
        '';
      in
      {
        devShells.default = pkgs.mkShell {
          buildInputs = with pkgs; [
            # Rust
            rustc
            cargo
            clippy
            rustfmt
            cargo-nextest
            cargo-flamegraph

            # JavaScript. pnpm tracks the `package.json#packageManager` major;
            # pnpm itself switches to that exact version when it differs.
            nodejs_22
            bun
            esbuild
            playwright-test
            pnpm_11

            # Native build dependencies
            cmake
            pkg-config
            openssl
            curl.dev
            llvmPackages.clang
            llvmPackages.libclang

            # Databases. Keep postgresql at the fixture server's major
            # (tests/testkit/src/postgres/Dockerfile) or the data suite refuses.
            postgresql_16
            sqlite

            # Test and CI tooling
            git
            jq
            lsof
            netcat
            net-tools
            procps
            which
            zstd
            actionlint

            # Load generators
            wrk
            hey

            # Repository test orchestration
            xtask
          ]
          ++ lib.optionals stdenv.hostPlatform.isLinux [ pkgs.perf pkgs.numactl ];

          RUST_BACKTRACE = "1";
          LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
          BINDGEN_EXTRA_CLANG_ARGS = lib.concatStringsSep " " bindgenClangArgs;
          PLAYWRIGHT_BROWSERS_PATH = "${playwright-driver.browsers}";
          PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD = "1";
          # Not exported; xtask/tests/playwright_browsers.rs reads its version.
          passthru = { inherit playwright-driver; };

          # The runtime's `wpt` test target `include_str!`s the pinned WPT tree
          # under crates/zeroship-runtime/tests/wpt. Link the flake's `wpt` input
          # there so the target compiles on a fresh checkout. A directory that is
          # not a WPT git checkout is left in place and reported, never deleted.
          shellHook = ''
            (
              root="$(git rev-parse --show-toplevel 2>/dev/null)" || exit 0
              link="$root/crates/zeroship-runtime/tests/wpt"
              target="${wpt}"
              if [ -L "$link" ] && [ "$(readlink "$link")" = "$target" ]; then
                exit 0
              fi
              if [ -e "$link" ] && [ ! -L "$link" ]; then
                if [ ! -d "$link/.git" ]; then
                  echo "zeroship: $link is a real directory that is not a WPT git checkout;" >&2
                  echo "zeroship: leaving it in place. Move it aside and re-enter this shell to" >&2
                  echo "zeroship: link the pinned WPT tree the flake provides." >&2
                  exit 0
                fi
                echo "zeroship: replacing the WPT git checkout at $link with the pinned tree." >&2
              fi
              rm -rf -- "$link"
              mkdir -p "$(dirname "$link")"
              ln -s -- "$target" "$link"
            )
          '';
        };
      });
}
