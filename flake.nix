{
  description = "OpenAPI Forge — plugin-driven OpenAPI code generator (`forge` CLI) and dev shell";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };
        rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
        # Nightly toolchain for `fuzz/`. cargo-fuzz / libFuzzer require
        # nightly; the rest of the workspace stays on stable. See
        # docs/fuzzing.md. `llvm-tools-preview` provides `llvm-cov` /
        # `llvm-profdata` so `cargo fuzz coverage` + `cargo cov` work.
        rustNightly = pkgs.rust-bin.selectLatestNightlyWith (t:
          t.default.override {
            extensions = [ "llvm-tools-preview" "rust-src" ];
          });

        # Build the host CLI (`forge`) with the workspace's pinned stable
        # toolchain so the package honours the repo MSRV / rust-toolchain.toml
        # rather than whatever rustc the host system happens to have.
        rustPlatform = pkgs.makeRustPlatform {
          cargo = rustToolchain;
          rustc = rustToolchain;
        };
        cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);

        forge = rustPlatform.buildRustPackage {
          pname = "openapi-forge-cli";
          version = cargoToml.workspace.package.version;
          src = self;

          cargoLock.lockFile = ./Cargo.lock;

          # The workspace also contains xtask, integration-test crates, etc.
          # Build and install only the host CLI, whose binary is `forge`.
          cargoBuildFlags = [ "-p" "openapi-forge-cli" ];

          # TLS goes through rustls, which pulls in aws-lc-sys: that needs cmake
          # and a C toolchain at build time. bindgenHook wires up libclang in
          # case aws-lc-sys has to generate its bindings for this platform.
          nativeBuildInputs = [
            pkgs.cmake
            pkgs.perl
            pkgs.pkg-config
            rustPlatform.bindgenHook
          ];

          # The CLI's tests (crates/forge-cli/tests/e2e.rs) exercise real OCI
          # registry flows and built wasm plugins; they don't run hermetically
          # in the Nix sandbox. The build still type-checks the whole binary.
          doCheck = false;

          meta = {
            description = "Plugin-driven OpenAPI code generator CLI";
            homepage = "https://github.com/marcusdunn/openapi-forge";
            license = with pkgs.lib.licenses; [ asl20 mit ];
            mainProgram = "forge";
          };
        };
      in {
        # `nix build` / `nix profile install` / referencing
        # `inputs.openapi-forge.packages.${system}.default` from another flake.
        packages = {
          default = forge;
          openapi-forge-cli = forge;
        };

        # `nix run github:marcusdunn/openapi-forge -- <args>`
        apps.default = {
          type = "app";
          program = "${forge}/bin/forge";
          meta.description = "Plugin-driven OpenAPI code generator CLI";
        };

        devShells.default = pkgs.mkShell {
          packages = [
            rustToolchain
            pkgs.cargo-nextest
            pkgs.cargo-component
            pkgs.cargo-deny
            pkgs.cargo-insta
            pkgs.wasm-tools
            pkgs.wasmtime
            pkgs.pkg-config
            pkgs.openssl
            # Go plugin toolchain (plugins/generator-go-server).
            # TinyGo ≥ 0.34 supports `-target=wasip2` natively; pair with
            # wit-bindgen-go (installed via `go install` in build.sh).
            pkgs.go
            pkgs.tinygo
            # Node + npm for the real-world itests. They type-check the
            # output of generator-typescript-fetch with `tsc`, and skip
            # that check when `tsc` is off PATH. Get it with
            # `npm install -g typescript`, as the CI job does.
            pkgs.nodejs_22
          ];

          RUST_SRC_PATH = "${rustToolchain}/lib/rustlib/src/rust/library";
        };

        # `nix develop .#fuzz` — nightly Rust + cargo-fuzz for working in
        # `fuzz/`. Kept separate so the default shell stays lean and the
        # main workspace never sees the nightly toolchain on PATH.
        devShells.fuzz = pkgs.mkShell {
          packages = [
            rustNightly
            pkgs.cargo-fuzz
            pkgs.pkg-config
            pkgs.openssl
          ];

          RUST_SRC_PATH = "${rustNightly}/lib/rustlib/src/rust/library";
        };
      });
}
