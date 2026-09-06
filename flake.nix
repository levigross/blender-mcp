{
  description = "Scheme-first Blender MCP server";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
    # Blender is pinned separately because the LTS release lands on unstable first:
    # nixos-26.05 currently carries 5.1.1, which is not an LTS. `checks.blender-lts`
    # fails the build if this input ever stops providing an LTS.
    nixpkgs-blender.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    # Crane takes its package set from `mkLib`, so it has no `nixpkgs` input to follow.
    crane.url = "github:ipetkov/crane";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    advisory-db = {
      url = "github:RustSec/advisory-db";
      flake = false;
    };
  };

  outputs = inputs @ { self, flake-parts, ... }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      systems = [ "x86_64-linux" "aarch64-linux" ];

      perSystem = { pkgs, system, ... }:
        let
          overlayPkgs = import inputs.nixpkgs {
            inherit system;
            overlays = [ inputs.rust-overlay.overlays.default ];
          };
          lib = overlayPkgs.lib;
          # The latest Blender LTS, pinned independently of the Rust toolchain.
          # `allowUnfree` is scoped to this import rather than asked of the caller:
          # `cudaSupport` sets WITH_CYCLES_DEVICE_OPTIX, and OptiX is redistributable
          # but unfree, so the package's licence gains `nvidiaCudaRedist`.
          blenderPkgs = import inputs.nixpkgs-blender {
            inherit system;
            config.allowUnfree = true;
          };
          # Without this Cycles enumerates the CPU and nothing else, and
          # `scene.cycles.device = "GPU"` silently renders on it anyway. Overriding the
          # one package keeps the blast radius smaller than a global `config.cudaSupport`,
          # though opensubdiv and openimagedenoise still rebuild alongside it.
          blender = blenderPkgs.blender.override { cudaSupport = true; };
          # The stock build, for when a GPU is irrelevant and the CUDA compile is not
          # worth the wait.
          blender-cpu = blenderPkgs.blender;
          rustToolchain = overlayPkgs.rust-bin.stable."1.98.0".default.override {
            extensions = [ "clippy" "rustfmt" "rust-src" ];
          };
          craneLib = (inputs.crane.mkLib overlayPkgs).overrideToolchain rustToolchain;
          # Cargo embeds Scheme and documentation, but never Python bridge code.
          # Keep those inputs separate so a Python-only fix reuses Rust artifacts.
          cargoSource = lib.cleanSourceWith {
            src = ./.;
            filter = path: type:
              let name = baseNameOf path;
              in !(builtins.elem name [ "target" "__pycache__" ".ruff_cache" "blender_extension" "scripts" ])
              && !(lib.hasSuffix ".so" path || lib.hasSuffix ".pyc" path)
              && ((craneLib.filterCargoSources path type)
              # `filterCargoSources` keeps only .rs and .toml, which would drop the
              # Scheme standard library that `worker.rs` pulls in with `include_str!`.
              || lib.hasSuffix ".scm" path
              || (lib.hasInfix "/docs/" path && lib.hasSuffix ".md" path));
          };
          extensionSource = lib.cleanSourceWith {
            src = ./blender_extension;
            filter = path: type:
              !(builtins.elem (baseNameOf path) [ "__pycache__" ".ruff_cache" ])
              && (type == "directory" || lib.hasSuffix ".py" path || lib.hasSuffix ".toml" path);
          };
          commonArgs = {
            src = cargoSource;
            strictDeps = true;
            nativeBuildInputs = [ overlayPkgs.pkg-config ];
            # The root Cargo.toml is a virtual workspace manifest with no `[package]`,
            # so crane cannot infer these and warns on every evaluation.
            pname = "blender-mcp";
            version = "0.1.0";
          };
          cargoArtifacts = craneLib.buildDepsOnly commonArgs;
          serverUnwrapped = craneLib.buildPackage (commonArgs // {
            inherit cargoArtifacts;
            cargoExtraArgs = "--package blender-mcp-server";
          });
          # The bridge's protocol, dispatcher, and catalog, compiled for Blender's own
          # interpreter. `abi3-py311` means one artifact serves any CPython from 3.11,
          # rather than tracking whichever version this Blender embeds.
          nativeModule = craneLib.buildPackage (commonArgs // {
            inherit cargoArtifacts;
            pname = "scheme-blender-mcp-native";
            cargoExtraArgs = "--package blender-mcp-native";
            # The test harness cannot link without libpython; `cargo-test-native`
            # covers this crate instead.
            doCheck = false;
            installPhaseCommand = ''
              mkdir -p "$out/lib"
              cp target/release/libscheme_blender_mcp_native.so \
                "$out/lib/scheme_blender_mcp_native.abi3.so"
            '';
          });
          extension = overlayPkgs.runCommand "scheme-blender-mcp-extension-0.1.0" {
            nativeBuildInputs = [ overlayPkgs.zip ];
            src = extensionSource;
          } ''
            mkdir -p "$out/share/blender-mcp"
            cp -R "$src/scheme_blender_mcp" "$out/share/blender-mcp/"
            chmod -R u+w "$out/share/blender-mcp/scheme_blender_mcp"
            cp ${nativeModule}/lib/scheme_blender_mcp_native.abi3.so \
              "$out/share/blender-mcp/scheme_blender_mcp/"
            cp ${./LICENSES/GPL-3.0-or-later.txt} \
              "$out/share/blender-mcp/scheme_blender_mcp/LICENSE"
            cp ${./LICENSE} \
              "$out/share/blender-mcp/scheme_blender_mcp/LICENSE-APACHE-2.0"
            # The headless backend imports the extension package from this script's
            # parent directory, so both must ship side by side.
            cp "$src/headless_bootstrap.py" "$out/share/blender-mcp/"
            cd "$out/share/blender-mcp"
            zip -qr "$out/share/blender-mcp/scheme_blender_mcp.zip" scheme_blender_mcp
          '';
          mkServer = blenderPackage: overlayPkgs.symlinkJoin {
            name = "blender-mcp-0.1.0";
            paths = [ serverUnwrapped extension ];
            nativeBuildInputs = [ overlayPkgs.makeWrapper ];
            postBuild = ''
              mkdir -p "$out/share/licenses/blender-mcp"
              cp ${./LICENSE} "$out/share/licenses/blender-mcp/Apache-2.0.txt"
              cp ${./LICENSES/GPL-3.0-or-later.txt} \
                "$out/share/licenses/blender-mcp/GPL-3.0-or-later.txt"
              wrapProgram "$out/bin/blender-mcp" \
                --prefix PATH : ${lib.makeBinPath [ blenderPackage ]} \
                --set-default BLENDER_MCP_BLENDER "${blenderPackage}/bin/blender" \
                --set-default BLENDER_MCP_EXTENSION_DIR "$out/share/blender-mcp/scheme_blender_mcp" \
                --set-default BLENDER_MCP_BOOTSTRAP "$out/share/blender-mcp/headless_bootstrap.py"
            '';
          };
          server = mkServer blender;
          serverCpu = mkServer blender-cpu;
          combined = overlayPkgs.runCommand "blender-mcp-distribution-0.1.0" { } ''
            mkdir -p "$out/bin" "$out/share/blender-mcp"
            cp -R ${server}/bin/. "$out/bin/"
            cp -R ${extension}/share/blender-mcp/. "$out/share/blender-mcp/"
            cp -R ${server}/share/licenses "$out/share/licenses"
            cp -R ${./docs} "$out/share/blender-mcp/docs"
            cp -R ${./examples} "$out/share/blender-mcp/examples"
          '';
          validate = overlayPkgs.writeShellApplication {
            name = "blender-mcp-validate";
            runtimeInputs = [ overlayPkgs.nix ];
            text = ''
              exec "${./scripts/validate.sh}" "$@"
            '';
          };
        in {
          packages = {
            default = server;
            blender-mcp = server;
            blender-mcp-cpu = serverCpu;
            blender-extension = extension;
            distribution = combined;
            # Exposed so scripts resolve the pinned LTS rather than whatever
            # `nixpkgs#blender` means in the caller's flake registry.
            inherit blender blender-cpu validate;
          };

          apps = {
            default = {
              type = "app";
              program = "${server}/bin/blender-mcp";
            };
            validate = {
              type = "app";
              program = "${validate}/bin/blender-mcp-validate";
            };
          };

          checks = {
            inherit server extension;
            python-lint = overlayPkgs.runCommand "blender-mcp-python-lint" {
              nativeBuildInputs = [ overlayPkgs.python3Packages.ruff ];
            } ''
              ruff check --no-cache ${extensionSource} ${./scripts/test_native.py} \
                ${./scripts/test_mcp.py} ${./scripts/test_bridge.py} ${./scripts/benchmark_mcp.py}
              touch "$out"
            '';
            bridge = overlayPkgs.runCommand "blender-mcp-bridge-tests" { } ''
              export BLENDER_MCP_EXTENSION_DIR="${extensionSource}/scheme_blender_mcp"
              ${overlayPkgs.python3}/bin/python ${./scripts/test_bridge.py}
              touch "$out"
            '';
            shell-lint = overlayPkgs.runCommand "blender-mcp-launcher-lint" {
              nativeBuildInputs = [ overlayPkgs.shellcheck ];
            } ''
              shellcheck ${./scripts/live.sh} ${./scripts/serve.sh}
              touch "$out"
            '';
            mcp-headless = overlayPkgs.runCommand "blender-mcp-headless-tests" { } ''
              ${overlayPkgs.python3}/bin/python ${./scripts/test_mcp.py} \
                ${serverCpu}/bin/blender-mcp
              touch "$out"
            '';
            mcp-benchmark = overlayPkgs.runCommand "blender-mcp-benchmark" { } ''
              mkdir -p "$out" "$TMPDIR/benchmark"
              cp ${./scripts/test_mcp.py} "$TMPDIR/benchmark/test_mcp.py"
              cp ${./scripts/benchmark_mcp.py} "$TMPDIR/benchmark/benchmark_mcp.py"
              ${overlayPkgs.python3}/bin/python "$TMPDIR/benchmark/benchmark_mcp.py" \
                ${serverCpu}/bin/blender-mcp "$out/result.json"
            '';
            source-layout = overlayPkgs.runCommand "blender-mcp-source-layout" { } ''
              test -f ${cargoSource}/Cargo.lock
              test -f ${cargoSource}/crates/blender-mcp-server/src/scheme/stdlib.scm
              test -f ${cargoSource}/docs/blender-api.md
              test ! -e ${cargoSource}/target
              test ! -e ${cargoSource}/blender_extension
              test ! -e ${cargoSource}/scripts
              test -f ${extensionSource}/scheme_blender_mcp/bridge.py
              test ! -e ${extensionSource}/scheme_blender_mcp/__pycache__
              touch "$out"
            '';
            blender-api = overlayPkgs.runCommand "blender-mcp-api-tests" { } ''
              export BLENDER_USER_CONFIG="$TMPDIR/config"
              export BLENDER_USER_SCRIPTS="$TMPDIR/scripts"
              export BLENDER_USER_EXTENSIONS="$TMPDIR/extensions"
              export BLENDER_MCP_EXTENSION_DIR="$TMPDIR/scheme_blender_mcp"
              cp -R ${extension}/share/blender-mcp/scheme_blender_mcp "$BLENDER_MCP_EXTENSION_DIR"
              chmod -R u+w "$BLENDER_MCP_EXTENSION_DIR"
              # ZIP cannot represent the Nix store's 1970 timestamps.
              find "$BLENDER_MCP_EXTENSION_DIR" -exec touch -h -t 198001010000 {} +
              export ALSOFT_DRIVERS=null
              ${blender-cpu}/bin/blender --background --factory-startup \
                --python-exit-code 1 --python ${./scripts/test_native.py}
              ${blender-cpu}/bin/blender --factory-startup --command extension validate \
                "$BLENDER_MCP_EXTENSION_DIR"
              ${blender-cpu}/bin/blender --factory-startup --command extension build \
                --source-dir "$BLENDER_MCP_EXTENSION_DIR" \
                --output-filepath "$TMPDIR/scheme_blender_mcp.zip"
              touch "$out"
            '';
            # `blender-mcp-native` is excluded because with `extension-module` PyO3
            # leaves libpython unlinked -- correct for a module loaded into Blender,
            # but the test harness is an executable and cannot link. It is covered by
            # `cargo-test-native` below, which links a real libpython instead.
            cargo-test = craneLib.cargoNextest (commonArgs // {
              inherit cargoArtifacts;
              cargoNextestExtraArgs = "--workspace --exclude blender-mcp-native";
            });
            cargo-test-native = craneLib.cargoNextest (commonArgs // {
              inherit cargoArtifacts;
              pname = "blender-mcp-native-tests";
              cargoNextestExtraArgs = "--package blender-mcp-native --no-default-features";
              # `strictDeps` keeps buildInputs off the build PATH, and PyO3 needs to
              # run an interpreter to configure itself, so this is a native input.
              nativeBuildInputs = commonArgs.nativeBuildInputs ++ [ overlayPkgs.python313 ];
            });
            cargo-clippy = craneLib.cargoClippy (commonArgs // {
              inherit cargoArtifacts;
              cargoClippyExtraArgs = "--workspace --all-targets -- --deny warnings";
            });
            cargo-doc = craneLib.cargoDoc (commonArgs // {
              inherit cargoArtifacts;
              cargoDocExtraArgs = "--workspace --no-deps";
              RUSTDOCFLAGS = "-D warnings";
            });
            cargo-fmt = craneLib.cargoFmt {
              src = cargoSource;
              inherit (commonArgs) pname version;
            };
            # Bans, licenses, and sources run offline against `deny.toml`.
            cargo-deny = craneLib.cargoDeny commonArgs;
            # Advisories need a database, which the sandbox cannot fetch, so they are
            # checked against the pinned `advisory-db` input instead.
            cargo-audit = craneLib.cargoAudit {
              inherit (commonArgs) src;
              inherit (inputs) advisory-db;
            };
            # Blender brands its long-term-support builds in the version string, so a
            # non-LTS pin fails here rather than silently shipping.
            blender-lts = overlayPkgs.runCommand "blender-is-lts" { } ''
              version="$(${blender}/bin/blender --version | head -n 1)"
              echo "$version"
              case "$version" in
                *LTS*) ;;
                *) echo "expected a Blender LTS release, got: $version" >&2; exit 1 ;;
              esac
              touch "$out"
            '';
          };

          devShells.default = craneLib.devShell {
            checks = self.checks.${system} or { };
            packages = [
              blender
              overlayPkgs.cargo-nextest
              overlayPkgs.cargo-deny
              overlayPkgs.python3Packages.pytest
              overlayPkgs.python3Packages.ruff
              overlayPkgs.jq
              overlayPkgs.just
              overlayPkgs.zip
            ];
            RUSTSEC_ADVISORY_DB = inputs.advisory-db;
          };
        };
    };
}
