{
  description = "Starla - A Rust implementation of the RIPE Atlas Software Probe";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
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

      in
      {
        packages = {
          default = pkgs.rustPlatform.buildRustPackage {
            pname = "starla";
            version = "6000.0.0";
            src = ./.;
            cargoLock.lockFile = ./Cargo.lock;

            inherit nativeBuildInputs;
            buildInputs = with pkgs; [ openssl ];

            # Skip tests during build (run separately in checks)
            doCheck = false;

            meta = with pkgs.lib; {
              description = "Starla - A Rust implementation of the RIPE Atlas Software Probe";
              homepage = "https://github.com/ananthb/starla";
              license = licenses.agpl3Only;
              maintainers = [ ];
            };
          };

          # Minimal build without observability features
          minimal = pkgs.rustPlatform.buildRustPackage {
            pname = "starla-minimal";
            version = "6000.0.0";
            src = ./.;
            cargoLock.lockFile = ./Cargo.lock;

            inherit nativeBuildInputs;
            buildInputs = with pkgs; [ openssl ];

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

        # CI checks for garnix
        checks = {
          # Formatting check
          formatting = pkgs.runCommand "check-formatting"
            {
              buildInputs = [ rustToolchain ];
              src = ./.;
            } ''
            cd $src
            cargo fmt --all -- --check
            touch $out
          '';

          # Clippy lints (all features)
          clippy = pkgs.runCommand "check-clippy"
            {
              buildInputs = nativeBuildInputs ++ buildInputs;
              src = ./.;
              OPENSSL_DIR = pkgs.openssl.dev;
              OPENSSL_LIB_DIR = "${pkgs.openssl.out}/lib";
            } ''
            cd $src
            export HOME=$(mktemp -d)
            cargo clippy --all-targets --all-features -- -D warnings
            touch $out
          '';

          # Clippy lints (minimal features)
          clippy-minimal = pkgs.runCommand "check-clippy-minimal"
            {
              buildInputs = nativeBuildInputs ++ buildInputs;
              src = ./.;
              OPENSSL_DIR = pkgs.openssl.dev;
              OPENSSL_LIB_DIR = "${pkgs.openssl.out}/lib";
            } ''
            cd $src
            export HOME=$(mktemp -d)
            cargo clippy --all-targets --no-default-features --features minimal -- -D warnings
            touch $out
          '';

          # Unit tests (all features)
          tests = pkgs.runCommand "check-tests"
            {
              buildInputs = nativeBuildInputs ++ buildInputs;
              src = ./.;
              OPENSSL_DIR = pkgs.openssl.dev;
              OPENSSL_LIB_DIR = "${pkgs.openssl.out}/lib";
            } ''
            cd $src
            export HOME=$(mktemp -d)
            cargo test --all-features --workspace
            touch $out
          '';

          # Unit tests (minimal features)
          tests-minimal = pkgs.runCommand "check-tests-minimal"
            {
              buildInputs = nativeBuildInputs ++ buildInputs;
              src = ./.;
              OPENSSL_DIR = pkgs.openssl.dev;
              OPENSSL_LIB_DIR = "${pkgs.openssl.out}/lib";
            } ''
            cd $src
            export HOME=$(mktemp -d)
            cargo test --no-default-features --features minimal --workspace
            touch $out
          '';

          # Build check (ensures the package builds)
          build = self.packages.${system}.default;
          build-minimal = self.packages.${system}.minimal;
        };

        devShells.default = pkgs.mkShell {
          name = "starla-v6000";

          buildInputs = [ rustToolchain ] ++ devPackages ++ buildInputs;

          shellHook = ''
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
    );
}
