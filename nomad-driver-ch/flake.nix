{
  description = "nomad-driver-ch dev shell — Go 1.25.x + delve + golangci-lint + gopls";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };
        # go_1_25 is in nixpkgs-unstable (verified 2026-05-23: go 1.25.9 on x86_64-linux).
        # The go.mod pin is `go 1.25.8`, so any 1.25.x toolchain is acceptable.
        # If a future nixpkgs bump drops go_1_25 before we move on, swap to `pkgs.go`
        # (whatever stable Go nixpkgs ships) and bump the README note.
        goToolchain = pkgs.go_1_25;
      in {
        devShells.default = pkgs.mkShell {
          buildInputs = [
            goToolchain
            pkgs.delve           # `dlv` — Go debugger
            pkgs.golangci-lint   # lint gate (matches `make lint`)
            pkgs.gopls           # LSP for editors
          ];

          shellHook = ''
            echo "nomad-driver-ch dev shell"
            go version
          '';
        };
      });
}
