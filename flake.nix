{
  description = "lait development environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
      flake-utils,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        };

        rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
      in
      {
        devShells.default = pkgs.mkShell {
          packages =
            [
              rustToolchain
              pkgs.cargo-make
              pkgs.cmake
              pkgs.pkg-config
            ]
            ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isDarwin [
              pkgs.libiconv
            ];

          # aws-lc-sys must link against the macOS SDK via Apple's clang; Nix's
          # own cc breaks that link, so pin the toolchain to the system clang
          # here the same way scripts/makers-cargo.sh does for non-Nix shells.
          shellHook = pkgs.lib.optionalString pkgs.stdenv.hostPlatform.isDarwin ''
            export CC=/usr/bin/clang
            export CXX=/usr/bin/clang++
            export SDKROOT="$(xcrun --sdk macosx --show-sdk-path)"
            case "$(uname -m)" in
              arm64|aarch64)
                export CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER=/usr/bin/clang
                ;;
              x86_64)
                export CARGO_TARGET_X86_64_APPLE_DARWIN_LINKER=/usr/bin/clang
                ;;
            esac
          '';
        };
      }
    );
}
