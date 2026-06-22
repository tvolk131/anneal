{
  description = "Anneal — native-tool-preserving polyglot build system (dev environment)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    let
      # A native-library toolchain (the `cargo_workspace` `native_libs` target):
      # a library a workspace links, exposed by manifest key. Bundles the lib's
      # closure (mounted read-only into the build) + `pkg-config` (so a `-sys`
      # crate's build script can discover it) + `PKG_CONFIG_PATH` env pointing at
      # the lib's `.pc` files. Consumers call this to add libpq / openssl / etc.
      # to their own manifest; `zlib` below is the worked example. Exposed as a
      # system-independent `lib` output (it takes `pkgs`).
      mkNativeLibToolchain = pkgs: libPkg:
        let dev = libPkg.dev or libPkg;
        in rec {
          packages = [ pkgs.pkg-config libPkg dev ];
          toolNames = [ "pkg-config" ];
          closure = pkgs.closureInfo { rootPaths = packages; };
          env = { PKG_CONFIG_PATH = "${dev}/lib/pkgconfig"; };
        };
    in
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };
        lib = pkgs.lib;

        rustToolPackages = with pkgs; [
          rustc
          cargo
          stdenv.cc
        ] ++ lib.optionals pkgs.stdenv.isDarwin (with pkgs; [
          xcbuild.xcrun
          # The macOS SDK as a pinned /nix/store input. rustc/cc resolve it via
          # DEVELOPER_DIR (set per cargo action from the manifest below); listing
          # it here makes its store path a declared rust-toolchain closure root,
          # so the sandbox mounts it read-only — no host SDK, fully hermetic.
          apple-sdk
        ]);
        rustToolNames = [ "cargo" "rustc" "cc" ]
          ++ lib.optionals pkgs.stdenv.isDarwin [ "xcrun" ];

        # Per-toolchain environment the manifest carries for rules to apply to
        # their actions (NOT general sandbox env). The rust toolchain needs
        # DEVELOPER_DIR on macOS so `xcrun`/rustc can locate the pinned SDK; the
        # value is a store path covered by the rust roots above, so it enters
        # both the toolchain identity and (when a rule sets it) the action key.
        rustEnvJson = builtins.toJSON (lib.optionalAttrs pkgs.stdenv.isDarwin {
          DEVELOPER_DIR = "${pkgs.apple-sdk}";
        });

        runtimeToolPackages = with pkgs; [
          bash
          coreutils
          curl
          gnugrep
          gnused
          gnutar
          # GNU tar's `z` shells out to an external gzip — without it in the
          # closure, `tar xzf` of a fetched .crate dies inside the sandbox.
          gzip
        ];
        runtimeToolNames = [
          "sh"
          "cat"
          "chmod"
          "cp"
          "curl"
          "grep"
          "gzip"
          "head"
          "mkdir"
          "sed"
          "tar"
        ];

        nodeToolPackages = with pkgs; [
          nodejs_22
          pnpm
        ];
        nodeToolNames = [ "pnpm" "node" ];

        nickelToolPackages = with pkgs; [
          nickel
        ];
        nickelToolNames = [ "nickel" ];

        # zlib: a real, broadly-useful native lib (libz-sys / flate2's zlib backend)
        # and the worked example exercised by the cargo_workspace native_libs tests.
        # Uses the `mkNativeLibToolchain` helper from the outer `let` (also exported).
        zlibLib = mkNativeLibToolchain pkgs pkgs.zlib;

        rustClosure = pkgs.closureInfo { rootPaths = rustToolPackages; };
        runtimeClosure = pkgs.closureInfo { rootPaths = runtimeToolPackages; };
        nodeClosure = pkgs.closureInfo { rootPaths = nodeToolPackages; };
        nickelClosure = pkgs.closureInfo { rootPaths = nickelToolPackages; };

        shellWordList = names: lib.concatStringsSep " " names;

        toolchainManifest = pkgs.runCommand "anneal-toolchains.json"
          {
            nativeBuildInputs =
              rustToolPackages
              ++ runtimeToolPackages
              ++ nodeToolPackages
              ++ nickelToolPackages
              ++ zlibLib.packages
              ++ [ pkgs.jq ];
          }
          ''
            set -eu

            store_root() {
              case "$1" in
                /nix/store/*)
                  entry="''${1#/nix/store/}"
                  printf '/nix/store/%s\n' "''${entry%%/*}"
                  ;;
                *)
                  echo "expected a /nix/store path, got $1" >&2
                  exit 1
                  ;;
              esac
            }

            json_tools() {
              first=1
              printf '{'
              for tool in "$@"; do
                path="$(command -v "$tool")"
                store_root "$path" >/dev/null
                if [ "$first" -eq 0 ]; then
                  printf ','
                fi
                first=0
                printf '"%s":"%s"' "$tool" "$path"
              done
              printf '}'
            }

            json_roots() {
              closure_file="$1"
              shift
              {
                cat "$closure_file"
                for tool in "$@"; do
                  store_root "$(command -v "$tool")"
                done
              } | sort -u | jq -R . | jq -s .
            }

            rust_tools="$(json_tools ${shellWordList rustToolNames})"
            rust_roots="$(json_roots ${rustClosure}/store-paths ${shellWordList rustToolNames})"
            runtime_tools="$(json_tools ${shellWordList runtimeToolNames})"
            runtime_roots="$(json_roots ${runtimeClosure}/store-paths ${shellWordList runtimeToolNames})"
            node_tools="$(json_tools ${shellWordList nodeToolNames})"
            node_roots="$(json_roots ${nodeClosure}/store-paths ${shellWordList nodeToolNames})"
            nickel_tools="$(json_tools ${shellWordList nickelToolNames})"
            nickel_roots="$(json_roots ${nickelClosure}/store-paths ${shellWordList nickelToolNames})"
            rust_env='${rustEnvJson}'
            zlib_tools="$(json_tools ${shellWordList zlibLib.toolNames})"
            zlib_roots="$(json_roots ${zlibLib.closure}/store-paths ${shellWordList zlibLib.toolNames})"
            zlib_env='${builtins.toJSON zlibLib.env}'

            jq -n \
              --argjson rust_tools "$rust_tools" \
              --argjson rust_roots "$rust_roots" \
              --argjson rust_env "$rust_env" \
              --argjson runtime_tools "$runtime_tools" \
              --argjson runtime_roots "$runtime_roots" \
              --argjson node_tools "$node_tools" \
              --argjson node_roots "$node_roots" \
              --argjson nickel_tools "$nickel_tools" \
              --argjson nickel_roots "$nickel_roots" \
              --argjson zlib_tools "$zlib_tools" \
              --argjson zlib_roots "$zlib_roots" \
              --argjson zlib_env "$zlib_env" \
              '{
                version: 1,
                toolchains: {
                  rust: {
                    tools: $rust_tools,
                    read_only_roots: $rust_roots,
                    env: $rust_env
                  },
                  "posix-runtime": {
                    tools: $runtime_tools,
                    read_only_roots: $runtime_roots,
                    env: {}
                  },
                  node: {
                    tools: $node_tools,
                    read_only_roots: $node_roots,
                    env: {}
                  },
                  nickel: {
                    tools: $nickel_tools,
                    read_only_roots: $nickel_roots,
                    env: {}
                  },
                  zlib: {
                    tools: $zlib_tools,
                    read_only_roots: $zlib_roots,
                    env: $zlib_env
                  }
                }
              }' > "$out"
          '';

        # The `anneal` CLI as an installable package, so another repo can take
        # this flake as an input and get the binary plus the toolchain manifest
        # (the two things a consumer needs — see packages below). Tests are
        # skipped: they exercise real cargo/pnpm against the network and the
        # sandbox, which the Nix build sandbox forbids.
        annealPackage = pkgs.rustPlatform.buildRustPackage {
          pname = "anneal";
          version = "0.0.0";
          src = self;
          cargoLock.lockFile = ./Cargo.lock;
          cargoBuildFlags = [ "-p" "anneal-cli" ];
          doCheck = false;
        };

        devShellPackages =
          rustToolPackages
          ++ runtimeToolPackages
          ++ nodeToolPackages
          ++ nickelToolPackages
          ++ (with pkgs; [
            # Rust developer tools
          clippy
          rustfmt
          rust-analyzer

            # General build/dev utilities
            git
            jq
          ])
          ++ lib.optionals pkgs.stdenv.isLinux (with pkgs; [
            bubblewrap
          ]);
      in
      {
        packages = {
          toolchain-manifest = toolchainManifest;
          anneal = annealPackage;
          default = annealPackage;
        };

        # `nix develop` / `nix develop --command <cmd>` gives a complete
        # contributor environment. The toolset is scoped to what the
        # Milestone 1 spikes and rules actually exercise:
        #   - Rust            : the build system itself + cargo_workspace
        #   - Nickel          : nickel_eval, the Nickel -> TS routing demo
        #   - Node + pnpm     : pnpm_workspace, the TS consumer side
        devShells.default = pkgs.mkShell {
          packages = devShellPackages;
          ANNEAL_TOOLCHAIN_MANIFEST = "${toolchainManifest}";

          shellHook = ''
            echo "anneal dev shell — rustc $(rustc --version | cut -d' ' -f2), nickel $(nickel --version 2>/dev/null | cut -d' ' -f2), node $(node --version), pnpm $(pnpm --version)"
          '';
        };
      }) // {
        # System-independent: the helper a consumer flake uses to add a native
        # library to its toolchain manifest (`mkNativeLibToolchain pkgs pkgs.postgresql`),
        # then references from a BUILD `cargo_workspace(native_libs = [...])`.
        lib.mkNativeLibToolchain = mkNativeLibToolchain;
      };
}
