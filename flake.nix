{
  description = "Starla - A Rust implementation of the RIPE Atlas Software Probe";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-utils.url = "github:numtide/flake-utils";
    git-hooks = {
      url = "github:cachix/git-hooks.nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils, git-hooks }:
    flake-utils.lib.eachDefaultSystem
      (system:
        let
          overlays = [ (import rust-overlay) ];
          pkgs = import nixpkgs {
            inherit system overlays;
          };

          rustToolchain = pkgs.rust-bin.nightly.latest.default.override {
            extensions = [ "rust-src" "rust-analyzer" "clippy" "rustfmt" ];
            targets = [
              "x86_64-unknown-linux-gnu"
              "aarch64-unknown-linux-gnu"
              "armv7-unknown-linux-gnueabihf"
            ];
          };

          # Common build inputs for the Rust package
          nativeBuildInputs = with pkgs; [
            pkg-config
            rustToolchain
            clang
            llvmPackages.libclang
          ];

          buildInputs = with pkgs; [
            openssl
            rocksdb
            libclang.lib
            llvmPackages.libclang
          ] ++ lib.optionals stdenv.isDarwin [
            darwin.apple_sdk.frameworks.Security
            darwin.apple_sdk.frameworks.SystemConfiguration
          ];

          # Development shell packages
          devPackages = with pkgs; [
            # Version control
            git
            git-lfs

            # Build essentials
            pkg-config
            openssl
            gcc
            gnumake

            # Database tools
            rocksdb
            llvmPackages.libclang

            # Performance analysis
            hyperfine
            tokei

            # Network tools (for testing)
            netcat
            curl

            # Utilities
            jq
            ripgrep
            fd
            bat
            eza
            direnv

            # Documentation
            mdbook
            graphviz

            # Cargo tools
            cargo-audit
            cargo-outdated
            cargo-watch
            cargo-tarpaulin
          ] ++ lib.optionals stdenv.isLinux [
            # Linux-specific
            iproute2
            tcpdump
          ];

          pre-commit-check = git-hooks.lib.${system}.run {
            src = ./.;
            hooks = {
              check-json.enable = true;
              check-merge-conflicts.enable = true;
              check-toml.enable = true;
              check-yaml.enable = true;
              detect-private-keys.enable = true;
              end-of-file-fixer.enable = true;
              mixed-line-endings.enable = true;
              trim-trailing-whitespace.enable = true;
              nixpkgs-fmt.enable = true;
              rustfmt = {
                enable = true;
                packageOverrides.cargo = rustToolchain;
                packageOverrides.rustfmt = rustToolchain;
              };
              clippy = {
                enable = true;
                packageOverrides.cargo = rustToolchain;
                packageOverrides.clippy = rustToolchain;
                settings.allFeatures = true;
                settings.denyWarnings = true;
              };
            };
          };

        in
        {
          packages = {
            default = pkgs.rustPlatform.buildRustPackage {
              pname = "starla";
              version = "0.1.0";
              src = ./.;
              cargoLock.lockFile = ./Cargo.lock;

              inherit nativeBuildInputs buildInputs;
              LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
              ROCKSDB_LIB_DIR = "${pkgs.rocksdb}/lib";

              doCheck = false;

              meta = with pkgs.lib; {
                description = "Starla - A Rust implementation of the RIPE Atlas Software Probe";
                homepage = "https://github.com/ananthb/starla";
                license = licenses.agpl3Only;
                maintainers = [ ];
              };
            };

            release =
              let
                pkg = self.packages.${system}.default;

                # Evaluate the NixOS module to extract the generated systemd unit
                evalCfg = (nixpkgs.lib.nixosSystem {
                  inherit system;
                  modules = [
                    self.nixosModules.default
                    {
                      services.starla.enable = true;
                      # Use a generic path for the release binary
                      services.starla.package = pkgs.writeShellScriptBin "starla" "";
                    }
                  ];
                }).config.systemd.services.starla;

                serviceFile = pkgs.writeText "starla.service" (
                  let
                    inherit (nixpkgs) lib;
                    sc = evalCfg.serviceConfig;
                    # Replace the nix-store ExecStart with a generic path
                    execStart = "/usr/bin/starla --config /etc/starla/config.toml";
                    listOrStr = v: if builtins.isList v then lib.concatStringsSep " " v else toString v;
                    boolStr = v: if v then "true" else "false";
                    fmtVal = v:
                      if builtins.isBool v then boolStr v
                      else listOrStr v;
                  in
                  lib.concatStringsSep "\n" ([
                    "[Unit]"
                    "Description=${evalCfg.description}"
                  ]
                  ++ map (a: "After=${a}") evalCfg.after
                  ++ map (w: "Wants=${w}") evalCfg.wants
                  ++ [ "" "[Service]" "ExecStart=${execStart}" ]
                  ++ lib.mapAttrsToList (k: v: "${k}=${fmtVal v}")
                    (builtins.removeAttrs sc [ "ExecStart" ])
                  ++ [ "" "[Install]" ]
                  ++ map (w: "WantedBy=${w}") evalCfg.wantedBy
                  ++ [ "" ])
                );
              in
              pkgs.runCommand "starla-release" { } ''
                mkdir -p $out
                cp ${pkg}/bin/starla $out/starla-x86_64-linux
                cp ${./config.toml.example} $out/config.toml.example
                cp ${serviceFile} $out/starla.service
              '';

            oci = pkgs.dockerTools.buildLayeredImage {
              name = "ghcr.io/ananthb/starla";
              tag = "latest";
              contents = [
                self.packages.${system}.default
                pkgs.cacert
              ];
              config = {
                Entrypoint = [ "/bin/starla" ];
                Env = [ "SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt" ];
              };
            };

            # Minimal build without observability features
            minimal = pkgs.rustPlatform.buildRustPackage {
              pname = "starla-minimal";
              version = "0.1.0";
              src = ./.;
              cargoLock.lockFile = ./Cargo.lock;

              inherit nativeBuildInputs buildInputs;
              LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
              ROCKSDB_LIB_DIR = "${pkgs.rocksdb}/lib";

              buildNoDefaultFeatures = true;
              buildFeatures = [ "minimal" ];

              doCheck = false;

              meta = with pkgs.lib; {
                description = "Starla (minimal build) - A Rust implementation of the RIPE Atlas Software Probe";
                homepage = "https://github.com/ananthb/starla";
                license = licenses.agpl3Only;
                maintainers = [ ];
              };
            };
          };

          # CI checks
          checks = {
            inherit pre-commit-check;

            # Build + clippy + tests (all features)

            default = pkgs.rustPlatform.buildRustPackage {
              pname = "starla-check";
              version = "0.0.0";
              src = ./.;
              cargoLock.lockFile = ./Cargo.lock;
              inherit nativeBuildInputs buildInputs;
              LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
              ROCKSDB_LIB_DIR = "${pkgs.rocksdb}/lib";
              buildPhase = ''
                export HOME=$(mktemp -d)
                cargo clippy --all-targets --all-features -- -D warnings
              '';
              doCheck = true;
              checkPhase = ''
                export HOME=$(mktemp -d)
                cargo test --all-features --workspace
              '';
              installPhase = "touch $out";
            };

            # Build + clippy + tests (minimal features)
            minimal = pkgs.rustPlatform.buildRustPackage {
              pname = "starla-check-minimal";
              version = "0.0.0";
              src = ./.;
              cargoLock.lockFile = ./Cargo.lock;
              inherit nativeBuildInputs buildInputs;
              LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
              ROCKSDB_LIB_DIR = "${pkgs.rocksdb}/lib";
              buildPhase = ''
                export HOME=$(mktemp -d)
                cargo clippy --all-targets --no-default-features --features minimal -- -D warnings
              '';
              doCheck = true;
              checkPhase = ''
                export HOME=$(mktemp -d)
                cargo test --no-default-features --features minimal --workspace
              '';
              installPhase = "touch $out";
            };

            # Full package builds
            build = self.packages.${system}.default;
            build-minimal = self.packages.${system}.minimal;
          };

          devShells.default = pkgs.mkShell {
            name = "starla-dev";

            buildInputs = [ rustToolchain ] ++ devPackages ++ buildInputs;

            shellHook = ''
              ${pre-commit-check.shellHook}
              cat << 'EOF'
              ================================================================
              Starla - Nix Development Environment
              ================================================================

              Quick Commands:
                cargo build --all-features           Build all workspace crates
                cargo test --all-features            Run all tests
                cargo clippy --all-features          Run clippy lints
                cargo fmt --all                      Format code

              Build Variants:
                cargo build --release --all-features              Release build
                cargo build --no-default-features --features minimal   Minimal build

              Cross-Compilation Targets:
                x86_64-unknown-linux-gnu
                aarch64-unknown-linux-gnu
                armv7-unknown-linux-gnueabihf

              Nix Commands:
                nix build                            Build the default package
                nix build .#minimal                  Build minimal variant
                nix flake check                      Run all CI checks

              EOF
              echo "Environment:"
              echo "  Rust:    $(rustc --version | cut -d' ' -f2)"
              echo "  Cargo:   $(cargo --version | cut -d' ' -f2)"
              echo ""
            '';

            # Environment variables
            RUST_BACKTRACE = "1";
            RUST_LOG = "info";
            CARGO_INCREMENTAL = "1";
            RUST_TEST_THREADS = "4";
            LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
            ROCKSDB_LIB_DIR = "${pkgs.rocksdb}/lib";
          };
        }
      ) // {
      nixosModules.default = { config, lib, pkgs, ... }: {
        imports = [ ./nix/module.nix ];
        config = lib.mkIf config.services.starla.enable {
          services.starla.package = lib.mkDefault self.packages.${pkgs.stdenv.hostPlatform.system}.default;
        };
      };
      nixosModules.starla = self.nixosModules.default;
    };
}
