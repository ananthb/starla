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
    # nix-appimage bundles the full closure into the squashfs and mounts
    # /nix/store via user namespaces at runtime, so the binary's ELF
    # interpreter and RUNPATH resolve on any modern Linux box. Replaces
    # the previous hand-rolled AppRun, which baked host /nix/store paths
    # into LD_LIBRARY_PATH while leaving PT_INTERP pointing at /nix/store
    # — the kernel couldn't exec it off the build host.
    nix-appimage = {
      url = "github:ralismark/nix-appimage";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.flake-utils.follows = "flake-utils";
    };
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils, git-hooks, nix-appimage }:
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

          # Static-only libscamperfile/libscamperctrl for linking into starla;
          # shipping only .a files forces rustc to static-link rscamper.
          scamper-static-libs = pkgs.stdenv.mkDerivation rec {
            pname = "scamper-static-libs";
            version = "20260420";
            src = pkgs.fetchurl {
              url = "https://www.caida.org/catalog/software/scamper/code/scamper-cvs-${version}.tar.gz";
              hash = "sha256-fW9rlOC4BDnkUhgxipLTBkWnvbsjxxH2hTbI8kP9Mxc=";
            };
            nativeBuildInputs = with pkgs; [ autoreconfHook pkg-config ];
            buildInputs = with pkgs; [ openssl ];
            configureFlags = [ "--disable-shared" "--enable-static" ];
            buildPhase = ''
              runHook preBuild
              make -C lib
              runHook postBuild
            '';
            installPhase = ''
              runHook preInstall
              mkdir -p $out/lib
              cp lib/libscamperfile/.libs/libscamperfile.a $out/lib/
              cp lib/libscamperctrl/.libs/libscamperctrl.a $out/lib/
              runHook postInstall
            '';
            doCheck = false;
            meta = with pkgs.lib; {
              description = "Static libscamperfile/libscamperctrl for linking into starla";
              homepage = "https://www.caida.org/catalog/software/scamper/";
              license = licenses.gpl2Only;
              platforms = platforms.linux;
            };
          };

          # Common build inputs for the Rust package.
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
            scamper-static-libs
          ];

          # rscamper's two static archives duplicate ~100 utils.o symbols;
          # tell ld to accept the first copy rather than error.
          rustFlagsLinux = pkgs.lib.optionalString pkgs.stdenv.isLinux
            "-C link-arg=-Wl,--allow-multiple-definition";

          # Fix rscamper 0.2.2's hardcoded i8 uses in the vendored source;
          # libc::c_char is u8 on aarch64 so the upstream build fails.
          rscamperPostPatch = ''
            for f in "$NIX_BUILD_TOP"/*/rscamper-*/src/inst.rs \
                     "$NIX_BUILD_TOP"/*/rscamper-*/src/file.rs; do
              [ -f "$f" ] || continue
              substituteInPlace "$f" \
                --replace-quiet "b'r' as i8" "b'r' as libc::c_char" \
                --replace-quiet "mode as i8" "mode as libc::c_char" \
                --replace-quiet "[0i8; 128]" "[0 as libc::c_char; 128]"
            done
          '';

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
              version = "0.6.3";
              src = ./.;
              cargoLock.lockFile = ./Cargo.lock;

              inherit nativeBuildInputs buildInputs;
              env.RUSTFLAGS = rustFlagsLinux;
              postPatch = rscamperPostPatch;

              doCheck = false;

              meta = with pkgs.lib; {
                description = "Starla - A Rust implementation of the RIPE Atlas Software Probe";
                homepage = "https://github.com/ananthb/starla";
                license = licenses.agpl3Only;
                maintainers = [ ];
              };
            };

            oci = pkgs.dockerTools.buildLayeredImage {
              name = "ghcr.io/ananthb/starla";
              tag = "latest";
              contents = [
                self.packages.${system}.default
                pkgs.cacert
                # Identify the image to starla's registration sub_arch
                # (read from /etc/os-release, matching the original C probe).
                (pkgs.writeTextDir "etc/os-release" ''
                  ID=starlaOCI
                  NAME="Starla Nix OCI image"
                '')
              ];
              config = {
                Entrypoint = [ "/bin/starla" ];
                Env = [ "SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt" ];
              };
            };

            # Minimal build without observability features
            minimal = pkgs.rustPlatform.buildRustPackage {
              pname = "starla-minimal";
              version = "0.6.3";
              src = ./.;
              cargoLock.lockFile = ./Cargo.lock;

              inherit nativeBuildInputs buildInputs;
              env.RUSTFLAGS = rustFlagsLinux;
              postPatch = rscamperPostPatch;

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

          } // {
            starla-tray = pkgs.rustPlatform.buildRustPackage {
              pname = "starla-tray";
              version = "0.6.3";
              src = ./.;
              cargoLock.lockFile = ./Cargo.lock;

              inherit nativeBuildInputs buildInputs;
              env.RUSTFLAGS = rustFlagsLinux;
              postPatch = rscamperPostPatch;

              cargoBuildFlags = [ "-p" "starla-tray" ];
              doCheck = false;

              postInstall = pkgs.lib.optionalString pkgs.stdenv.isDarwin ''
                # macOS .app bundle for the tray: gives it a dock icon,
                # proper app lifecycle, and allows launchd to manage it.
                mkdir -p "$out/Applications/Starla Tray.app/Contents/MacOS"
                mkdir -p "$out/Applications/Starla Tray.app/Contents/Resources"
                cp $src/packaging/Info.plist "$out/Applications/Starla Tray.app/Contents/"
                cp $src/assets/logo.icns "$out/Applications/Starla Tray.app/Contents/Resources/icon.icns"
                cp $out/bin/starla-tray "$out/Applications/Starla Tray.app/Contents/MacOS/"
              '';

              meta = with pkgs.lib; {
                description = "Starla system tray app";
                homepage = "https://github.com/ananthb/starla";
                license = licenses.agpl3Only;
                maintainers = [ ];
              };
            };

          } // pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
            release =
              let
                pkg = self.packages.${system}.default;
                tray = self.packages.${system}.starla-tray;
                arch = if system == "x86_64-linux" then "amd64" else "arm64";
              in
              pkgs.runCommand "starla-${arch}.tar.gz"
                {
                  nativeBuildInputs = [ pkgs.gzip pkgs.patchelf ];
                } ''
                mkdir -p starla
                cp ${pkg}/bin/starla starla/starla
                cp ${tray}/bin/starla-tray starla/starla-tray
                chmod +w starla/starla starla/starla-tray
                patchelf --remove-rpath starla/starla
                patchelf --remove-rpath starla/starla-tray
                cp ${./config.toml.example} starla/config.toml.example
                cp ${./starla.service} starla/starla.service
                cp ${./packaging/starla-tray.desktop} starla/starla-tray.desktop
                tar -czvf $out -C . starla
              '';

            appimage = nix-appimage.lib.${system}.mkAppImage {
              program = "${self.packages.${system}.starla-tray}/bin/starla-tray";
              pname = "starla-tray";
              name = "starla-tray-${if system == "x86_64-linux" then "x86_64" else "aarch64"}.AppImage";
            };
          } // pkgs.lib.optionalAttrs pkgs.stdenv.isDarwin {
            release =
              let
                pkg = self.packages.${system}.default;
                tray = self.packages.${system}.starla-tray;
                arch = if system == "x86_64-darwin" then "amd64" else "arm64";
              in
              pkgs.runCommand "starla-macos-${arch}.dmg"
                {
                  nativeBuildInputs = [ pkgs.cctools ];
                }
                ''
                  export PATH="/usr/bin:$PATH"
                  mkdir -p staging

                  # Copy the .app bundle from the tray package
                  cp -rL "${tray}/Applications/Starla Tray.app" staging/
                  chmod -R u+w staging/

                  # Add the CLI probe binary into the .app bundle
                  cp ${pkg}/bin/starla "staging/Starla Tray.app/Contents/MacOS/"

                  # Rewrite any /nix/store dylib references to /usr/lib
                  # so binaries work on non-Nix macOS systems.
                  for bin in "staging/Starla Tray.app/Contents/MacOS/starla" \
                             "staging/Starla Tray.app/Contents/MacOS/starla-tray"; do
                    chmod +w "$bin"
                    for dep in $(otool -L "$bin" | grep /nix/store | awk '{print $1}'); do
                      base=$(basename "$dep")
                      install_name_tool -change "$dep" "/usr/lib/$base" "$bin"
                    done

                    # Fail the build if anything else slipped through — a
                    # /nix/store LC_LOAD_DYLIB or LC_RPATH will make dyld
                    # abort on user Macs. Catching it here is much cheaper
                    # than catching it from a bug report.
                    if otool -L "$bin" | grep -q '/nix/store'; then
                      echo "ERROR: $bin still references /nix/store dylibs:" >&2
                      otool -L "$bin" | grep '/nix/store' >&2
                      exit 1
                    fi
                    if otool -l "$bin" | grep -A2 LC_RPATH | grep -q '/nix/store'; then
                      echo "ERROR: $bin has /nix/store in LC_RPATH:" >&2
                      otool -l "$bin" | grep -A2 LC_RPATH >&2
                      exit 1
                    fi
                  done

                  # Include config example and launchd plist
                  cp ${./config.toml.example} staging/config.toml.example
                  cp ${./packaging/com.ananthb.starla.plist} staging/com.ananthb.starla.plist

                  # Install CLI script that symlinks the probe binary
                  cat > staging/Install\ CLI.command << 'SCRIPT'
                  #!/bin/bash
                  set -e
                  dst="/usr/local/bin/starla"
                  src="/Applications/Starla Tray.app/Contents/MacOS/starla"
                  if [ ! -f "$src" ]; then
                    echo "Error: Starla Tray.app not found in /Applications."
                    echo "Drag the app to Applications first, then run this again."
                    exit 1
                  fi
                  mkdir -p /usr/local/bin
                  ln -sf "$src" "$dst"
                  echo "Installed: $dst -> $src"
                  SCRIPT
                  chmod +x staging/Install\ CLI.command

                  # Applications symlink for drag-and-drop install
                  ln -s /Applications staging/Applications

                  # `hdiutil create -srcfolder` intermittently fails with
                  # `Resource busy` on GitHub macOS runners — the internal
                  # attach/convert step races with mds / runner agents that
                  # touch the staging dir. A short retry covers it.
                  attempt=1
                  until hdiutil create -volname "Starla" -srcfolder staging \
                    -ov -format UDZO "$out"; do
                    if [ "$attempt" -ge 3 ]; then
                      echo "hdiutil create failed after $attempt attempts" >&2
                      exit 1
                    fi
                    echo "hdiutil create failed (attempt $attempt), retrying..." >&2
                    rm -f "$out"
                    sleep $((attempt * 5))
                    attempt=$((attempt + 1))
                  done
                '';
          };

          # CI checks
          checks = {
            inherit pre-commit-check;

            # Build + clippy + tests with default features.
            default = pkgs.rustPlatform.buildRustPackage {
              pname = "starla-check";
              version = "0.0.0";
              src = ./.;
              cargoLock.lockFile = ./Cargo.lock;
              inherit nativeBuildInputs buildInputs;
              env.RUSTFLAGS = rustFlagsLinux;
              postPatch = rscamperPostPatch;
              buildPhase = ''
                export HOME=$(mktemp -d)
                cargo clippy --all-targets -- -D warnings
              '';
              doCheck = true;
              checkPhase = ''
                export HOME=$(mktemp -d)
                cargo test --workspace
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
              env.RUSTFLAGS = rustFlagsLinux;
              postPatch = rscamperPostPatch;
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
            RUSTFLAGS = rustFlagsLinux;
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
