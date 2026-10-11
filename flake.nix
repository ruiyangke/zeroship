{
  description = "zeroship - AI-native app platform";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
    flake-utils.url = "github:numtide/flake-utils";
    # Nightly Rust for cargo-fuzz's `-Zsanitizer`. Applied only to the `fuzz`
    # development shell below, so the default shell keeps the nixpkgs stable
    # toolchain. `follows` pins the overlay's nixpkgs to this flake's.
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
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

  outputs = { self, nixpkgs, flake-utils, rust-overlay, wpt }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        inherit (pkgs) lib stdenv;

        # cargo-fuzz needs a nightly compiler for `-Zsanitizer=address`, which
        # stable cannot accept. The overlay is applied to its OWN package set so
        # the default shell's `rustc`/`cargo` stay exactly the nixpkgs stable
        # versions. The date pins the toolchain; `rust-src` and `llvm-tools` are
        # the components cargo-fuzz's sanitizer build reads.
        fuzzPkgs = pkgs.extend rust-overlay.overlays.default;
        fuzzToolchain = fuzzPkgs.rust-bin.nightly."2026-10-02".default.override {
          extensions = [ "rust-src" "llvm-tools" ];
        };

        # Miri catches undefined behaviour by interpreting the pure unsafe code
        # a package's unit tests reach. It needs the same nightly pin as `fuzz`
        # plus the `miri` component and `rust-src` (Miri builds its own sysroot
        # from the source). A separate shell keeps the default shell's stable
        # toolchain and the fuzz shell's component set untouched.
        miriToolchain = fuzzPkgs.rust-bin.nightly."2026-10-02".default.override {
          extensions = [ "rust-src" "miri" ];
        };

        # The creator CLI ships for macOS and Windows as well as Linux. This
        # toolchain is the default shell's compiler release, read from
        # nixpkgs' `rustc`, with the standard libraries of the CLI's other
        # release targets added, so a cross-target `cargo check` on a Linux
        # host type-checks the same compiler CI builds those targets with.
        crossTargets = [ "aarch64-apple-darwin" "x86_64-pc-windows-msvc" ];
        crossToolchain = fuzzPkgs.rust-bin.stable.${pkgs.rustc.version}.default.override {
          targets = crossTargets;
        };

        # A C and C++ compiler for the Windows MSVC target's C dependencies in a
        # check. There are no MSVC headers here, so zig compiles them against
        # the MinGW headers it carries, dropping the MSVC target flag cc-rs
        # adds; the objects are never linked, so a check that passes proves the
        # Rust code, not the C build, which CI's Windows job owns.
        windowsCheckCompiler = name: driver: pkgs.writeShellScript name ''
          args=()
          for arg in "$@"; do
            case "$arg" in
              --target=*|-m64) ;;
              *) args+=("$arg") ;;
            esac
          done
          exec ${fuzzPkgs.zig}/bin/zig ${driver} -target x86_64-windows-gnu "''${args[@]}"
        '';
        windowsCheckArchiver = pkgs.writeShellScript "windows-check-lib" ''
          exec ${fuzzPkgs.zig}/bin/zig lib "$@"
        '';

        # bindgen (libsqlite3-sys, pg_query, v8) drives libclang directly, so
        # the cc wrapper's flags never reach it: name the system headers here.
        # Darwin names them as a sysroot, since libSystem carries no headers.
        bindgenClangArgs =
          lib.optionals stdenv.hostPlatform.isLinux [ "-isystem ${stdenv.cc.libc.dev}/include" ]
          ++ lib.optionals stdenv.hostPlatform.isDarwin [ "-isysroot ${pkgs.apple-sdk.sdkroot}" ];

        # The workspace Playwright catalog tracks this driver's version; see
        # xtask/tests/playwright/mod.rs.
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
            # (zeroship_testkit_server::images::PGVECTOR_16) or the data shard refuses.
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
            # All-thread stacks of a hung test process (`gdb -p <pid> -batch -ex "thread apply all bt"`).
            gdb

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
          # Not exported; xtask/tests/playwright/mod.rs reads its version.
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

        # Fuzzing only. Nowhere else may take the nightly toolchain: the
        # sanitizer it exists for is a cargo-fuzz build detail, and the shipped
        # workspace keeps the stable shell. Run a target from the crate root:
        #
        #   nix develop .#fuzz --command \
        #     cargo fuzz run backend_message -- -max_total_time=60
        devShells.fuzz = fuzzPkgs.mkShell {
          buildInputs = with fuzzPkgs; [
            fuzzToolchain
            cargo-fuzz
            # libfuzzer-sys compiles the bundled runtime with a C++ compiler,
            # and the sanitizer link needs this LLVM.
            llvmPackages.clang
            llvmPackages.libclang

            # The crate under test builds with the same native inputs as the
            # default shell.
            cmake
            pkg-config
            openssl
            curl.dev
            git
            which
            zstd
          ];

          RUST_BACKTRACE = "1";
          LIBCLANG_PATH = "${fuzzPkgs.llvmPackages.libclang.lib}/lib";
          # LeakSanitizer suspends threads through ptrace, which the kernel
          # refuses to a process that is not the tracer when
          # kernel.yama.ptrace_scope is 2: it aborts at exit with "fatal error"
          # on a run that otherwise completed. These targets exist for panics
          # and hangs, so leak detection is off rather than reporting the
          # sandbox's ptrace policy as a decoder crash.
          ASAN_OPTIONS = "detect_leaks=0";
        };

        # Miri only. Run a package's pure-unsafe unit tests through the flake
        # shell so the toolchain is the one definition and nothing is installed
        # ad hoc:
        #
        #   nix develop .#miri --command \
        #     cargo miri test -p compio-postgres --lib <filter>
        #
        # The native build inputs match the default shell's, because Miri still
        # compiles the package and its build scripts; the `miri` component is
        # what differs.
        devShells.miri = fuzzPkgs.mkShell {
          buildInputs = with fuzzPkgs; [
            miriToolchain

            cmake
            pkg-config
            openssl
            curl.dev
            llvmPackages.clang
            llvmPackages.libclang

            git
            which
            zstd
          ];

          RUST_BACKTRACE = "1";
          LIBCLANG_PATH = "${fuzzPkgs.llvmPackages.libclang.lib}/lib";
          BINDGEN_EXTRA_CLANG_ARGS = lib.concatStringsSep " " bindgenClangArgs;
        };

        # Cross-target type checks of the creator CLI from a Linux host:
        #
        #   nix develop .#cross --command cargo-zigbuild check --locked \
        #     -p zeroship-cli --all-targets --target aarch64-apple-darwin
        #   nix develop .#cross --command cargo check --locked \
        #     -p zeroship-cli --all-targets --target x86_64-pc-windows-msvc
        #
        # cargo-zigbuild compiles the macOS C dependencies with zig, which
        # carries the macOS libc headers but no Apple framework. A check never
        # links, so the one framework header on the path, aws-lc's jitter
        # entropy source (`CoreServices`), is left out of the macOS check
        # below. The Windows check compiles its C dependencies with the zig
        # wrappers above, and aws-lc with its plain C builder and the assembly
        # objects its crate ships, since there is no NASM or MSVC toolchain
        # here. Both checks prove the Rust code; CI's `portable` jobs build and
        # link everything on the real systems.
        devShells.cross = fuzzPkgs.mkShell {
          buildInputs = with fuzzPkgs; [
            crossToolchain
            cargo-zigbuild
            zig

            cmake
            pkg-config
            llvmPackages.libclang

            git
            which
          ];

          RUST_BACKTRACE = "1";
          LIBCLANG_PATH = "${fuzzPkgs.llvmPackages.libclang.lib}/lib";
          AWS_LC_SYS_NO_JITTER_ENTROPY_aarch64_apple_darwin = "1";
          CC_x86_64_pc_windows_msvc = windowsCheckCompiler "windows-check-cc" "cc";
          CXX_x86_64_pc_windows_msvc = windowsCheckCompiler "windows-check-cxx" "c++";
          AR_x86_64_pc_windows_msvc = windowsCheckArchiver;
          AWS_LC_SYS_CMAKE_BUILDER_x86_64_pc_windows_msvc = "0";
          AWS_LC_SYS_PREBUILT_NASM_x86_64_pc_windows_msvc = "1";
        };
      });
}
