{
  description = "zeroship - AI-native app platform";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
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
            # (tests/fixtures/postgres/Dockerfile) or the data suite refuses.
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

            # Load generators
            wrk
            hey
          ]
          ++ lib.optionals stdenv.hostPlatform.isLinux [ pkgs.perf pkgs.numactl ];

          RUST_BACKTRACE = "1";
          LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
          BINDGEN_EXTRA_CLANG_ARGS = lib.concatStringsSep " " bindgenClangArgs;
          PLAYWRIGHT_BROWSERS_PATH = "${pkgs.playwright-driver.browsers}";
          PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD = "1";
        };
      });
}
