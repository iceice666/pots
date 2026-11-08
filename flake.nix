{
  description = "Pots：本機或自架的 agent 工作台（Rust workspace + GPUI 桌面客戶端）";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
    }:
    let
      systems = [
        "aarch64-darwin"
        "x86_64-darwin"
        "aarch64-linux"
        "x86_64-linux"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f system);

      # GPUI（gpui-pre-macos 與 gpui-kit）用到穩定版還沒有的語言特性，
      # 所以工具鏈是 nightly。版本跟著 rust-overlay 的 nightly channel 走，
      # 需要的話改成 `rust-bin.nightly."2026-11-01"` 之類的日期釘死。
      rustFor =
        pkgs:
        pkgs.rust-bin.nightly.latest.default.override {
          extensions = [
            "rust-src"
            "rust-analyzer"
            "clippy"
            "rustfmt"
          ];
        };
    in
    {
      devShells = forAllSystems (
        system:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ rust-overlay.overlays.default ];
          };
          rust = rustFor pkgs;
        in
        {
          default = pkgs.mkShell {
            packages = [
              rust
              # aws-lc-sys（jsonwebtoken 的密碼學後端）用 cmake 與 perl 建置。
              pkgs.cmake
              pkgs.perl
              pkgs.pkg-config
            ]
            # Linux 的 GPUI（WGPU）需要這些系統函式庫。
            ++ pkgs.lib.optionals pkgs.stdenv.isLinux [
              pkgs.vulkan-loader
              pkgs.libxkbcommon
              pkgs.wayland
              pkgs.libGL
              pkgs.fontconfig
            ];

            env = {
              RUST_SRC_PATH = "${rust}/lib/rustlib/src/rust/library";
            };

            shellHook = ''
              echo "pots dev shell：$(rustc --version)"
              echo "  cargo check --workspace     # 服務、協定、模型層與桌面客戶端"
              echo "  cargo test -p pots-server   # 服務整合測試"
            '';
          };
        }
      );

      formatter = forAllSystems (
        system:
        let
          pkgs = import nixpkgs { inherit system; };
        in
        pkgs.nixfmt-rfc-style
      );
    };
}
