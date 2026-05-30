{
  description = "Anneal — native-tool-preserving polyglot build system (dev environment)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };
      in
      {
        # `nix develop` / `nix develop --command <cmd>` gives a complete
        # contributor environment. The toolset is scoped to what the
        # Milestone 1 spikes and rules actually exercise:
        #   - Rust            : the build system itself + cargo_workspace
        #   - Nickel          : nickel_eval, the Nickel -> TS routing demo
        #   - Node + pnpm     : pnpm_workspace, the TS consumer side
        devShells.default = pkgs.mkShell {
          packages = with pkgs; [
            # Rust toolchain
            rustc
            cargo
            clippy
            rustfmt
            rust-analyzer

            # Nickel (generated-native-package source for the routing demo)
            nickel

            # Node / pnpm (TS consumer side of the cross-language boundary)
            nodejs_22
            pnpm

            # General build/dev utilities
            git
            jq
          ];

          shellHook = ''
            echo "anneal dev shell — rustc $(rustc --version | cut -d' ' -f2), nickel $(nickel --version 2>/dev/null | cut -d' ' -f2), node $(node --version), pnpm $(pnpm --version)"
          '';
        };
      });
}
