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
            sqlite
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
          # Playwright: use Nix-provided browsers, npm provides the test runner
          PLAYWRIGHT_BROWSERS_PATH = "${pkgs.playwright-driver.browsers}";
          PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD = "1";
        };
      });
}
