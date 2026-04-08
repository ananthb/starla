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
          ];

          buildInputs = with pkgs; [
            openssl
          ] ++ lib.optionals stdenv.isLinux [
            glib
            gtk3
            libayatana-appindicator
            xdotool
          ] ++ lib.optionals stdenv.isDarwin [
            darwin.apple_sdk.frameworks.Security
            darwin.apple_sdk.frameworks.SystemConfiguration
            darwin.apple_sdk.frameworks.AppKit
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

            # Signing
            cosign

            # Fuzzing
            cargo-fuzz
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
              # clippy is handled by checks.default and checks.minimal
              # which use buildRustPackage with vendored dependencies.
              # The pre-commit hook can't fetch crates in the nix sandbox.
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
                arch = if system == "x86_64-linux" then "amd64" else "arm64";
              in
              pkgs.runCommand "starla-${arch}.tar.gz"
                {
                  nativeBuildInputs = [ pkgs.gzip ];
                } ''
                mkdir -p starla
                cp ${pkg}/bin/starla starla/starla
                cp ${./config.toml.example} starla/config.toml.example
                cp ${./starla.service} starla/starla.service
                tar -czvf $out -C . starla
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
          } // pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
            starla-tray = pkgs.rustPlatform.buildRustPackage {
              pname = "starla-tray";
              version = "0.1.0";
              src = ./.;
              cargoLock.lockFile = ./Cargo.lock;

              inherit nativeBuildInputs buildInputs;

              cargoBuildFlags = [ "-p" "starla-tray" ];
              doCheck = false;

              meta = with pkgs.lib; {
                description = "Starla system tray app";
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

      darwinModules.default = { config, lib, pkgs, ... }: {
        imports = [ ./nix/darwin-module.nix ];
        config = lib.mkIf config.services.starla.enable {
          services.starla.package = lib.mkDefault self.packages.${pkgs.stdenv.hostPlatform.system}.default;
        };
      };
      darwinModules.starla = self.darwinModules.default;

      homeManagerModules.default = { config, lib, pkgs, ... }: {
        imports = [ ./nix/home-module.nix ];
        config = lib.mkIf config.services.starla.enable {
          services.starla.package = lib.mkDefault self.packages.${pkgs.stdenv.hostPlatform.system}.default;
          services.starla.trayPackage = lib.mkDefault self.packages.${pkgs.stdenv.hostPlatform.system}.starla-tray;
        };
      };
      homeManagerModules.starla = self.homeManagerModules.default;
    };
}
