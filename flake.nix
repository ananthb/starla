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
              version = "0.3.0";
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
              version = "0.3.0";
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
            starla-tray = pkgs.rustPlatform.buildRustPackage {
              pname = "starla-tray";
              version = "0.3.0";
              src = ./.;
              cargoLock.lockFile = ./Cargo.lock;

              inherit nativeBuildInputs buildInputs;

              cargoBuildFlags = [ "-p" "starla-tray" ];
              doCheck = false;

              postInstall = pkgs.lib.optionalString pkgs.stdenv.isDarwin ''
                # macOS .app bundle for the tray — gives it a dock icon,
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

            appimage =
              let
                tray = self.packages.${system}.starla-tray;
                arch = if system == "x86_64-linux" then "x86_64" else "aarch64";

                appimageRuntime = pkgs.fetchurl (if system == "x86_64-linux" then {
                  url = "https://github.com/AppImage/type2-runtime/releases/download/20251108/runtime-x86_64";
                  hash = "sha256-L8qLRDySUQ8Ug6iD9gBhrQm0a5eLJjHIB82HOkfsJg0=";
                } else {
                  url = "https://github.com/AppImage/type2-runtime/releases/download/20251108/runtime-aarch64";
                  hash = "sha256-AMvfz5F8xsD/bTNH1Z4Moff0Wm3xpCig1tinhmTYdEQ=";
                });

                libDeps = with pkgs; [
                  glib
                  gtk3
                  libayatana-appindicator
                  pango
                  cairo
                  gdk-pixbuf
                  atk
                  harfbuzz
                  fontconfig
                  freetype
                  xorg.libX11
                  xorg.libXcursor
                  xorg.libXrandr
                  xorg.libXi
                  xorg.libXext
                  xorg.libXrender
                  xorg.libXfixes
                  xorg.libXcomposite
                  xorg.libXdamage
                  xorg.libxcb
                  libxkbcommon
                  wayland
                  xdotool
                ];
              in
              pkgs.runCommand "starla-tray-${arch}.AppImage"
                {
                  nativeBuildInputs = with pkgs; [ squashfsTools patchelf ];
                } ''
                mkdir -p AppDir/usr/bin
                mkdir -p AppDir/usr/lib
                mkdir -p AppDir/usr/share/applications

                cp ${tray}/bin/starla-tray AppDir/usr/bin/
                chmod +w AppDir/usr/bin/*
                patchelf --remove-rpath AppDir/usr/bin/starla-tray

                # Bundle shared libraries so the AppImage is self-contained.
                for dir in ${pkgs.lib.concatStringsSep " " (map (d: "${d}/lib") libDeps)}; do
                  if [ -d "$dir" ]; then
                    for so in "$dir"/*.so "$dir"/*.so.*; do
                      [ -e "$so" ] || continue
                      cp -n "$(readlink -f "$so")" "AppDir/usr/lib/$(basename "$so")" 2>/dev/null || true
                    done
                  fi
                done

                cp ${./packaging/starla-tray.desktop} AppDir/starla-tray.desktop
                cp ${./packaging/starla-tray.desktop} AppDir/usr/share/applications/

                cat > AppDir/AppRun << 'APPRUN'
                #!/bin/bash
                set -e
                SELF=$(readlink -f "$0")
                APPDIR=''${SELF%/*}
                export LD_LIBRARY_PATH="''${APPDIR}/usr/lib:''${LD_LIBRARY_PATH}"
                export GSETTINGS_SCHEMA_DIR="/usr/share/glib-2.0/schemas:''${GSETTINGS_SCHEMA_DIR}"
                exec "''${APPDIR}/usr/bin/starla-tray" "$@"
                APPRUN
                chmod +x AppDir/AppRun

                mksquashfs AppDir appimage.squashfs -root-owned -noappend -comp zstd -quiet -no-progress
                cat ${appimageRuntime} appimage.squashfs > $out
                chmod +x $out
              '';
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

                  hdiutil create -volname "Starla" -srcfolder staging \
                    -ov -format UDZO $out
                '';
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
