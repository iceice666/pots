{
  description = "pots-rework dev shell with an up-to-date Rust toolchain";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    { nixpkgs, rust-overlay, ... }:
    let
      systems = [
        "aarch64-darwin"
        "x86_64-darwin"
        "aarch64-linux"
        "x86_64-linux"
      ];
      forAllSystems =
        f:
        nixpkgs.lib.genAttrs systems (
          system:
          f (
            import nixpkgs {
              inherit system;
              overlays = [ rust-overlay.overlays.default ];
            }
          )
        );
    in
    {
      devShells = forAllSystems (
        pkgs:
        let
          # Latest stable at the time flake.lock was updated; bump with `nix flake update`.
          rust = pkgs.rust-bin.stable.latest.default.override {
            extensions = [
              "rust-src"
              "rust-analyzer"
              "clippy"
              "rustfmt"
            ];
          };

          # gpui's X11/Wayland backends on Linux.
          linuxLibs = with pkgs; [
            fontconfig
            freetype
            libxkbcommon
            vulkan-loader
            wayland
            libx11
            libxcb
            libxcursor
            libxi
            libxrandr
            openssl
            alsa-lib
          ];
        in
        {
          # On macOS, rust-bin already brings nixpkgs' clang + apple-sdk (which has the
          # Metal/AppKit frameworks gpui needs), so a separate stdenv CC isn't required.
          default =
            (if pkgs.stdenv.hostPlatform.isDarwin then pkgs.mkShellNoCC else pkgs.mkShell) {
              packages = [ rust ] ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux ([ pkgs.pkg-config ] ++ linuxLibs);

              env = pkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
                LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath linuxLibs;
              };
            };
        }
      );
    };
}
