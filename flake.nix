{
  description = "datavis — real-time data visualization tool (egui + ZMQ + Zarr)";

  nixConfig = {
    extra-substituters = [];
    extra-trusted-public-keys = [];
  };

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
      systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
      forEachSystem = f: builtins.listToAttrs (map (system: { name = system; value = f system; }) systems);
    in
    {
      devShells = forEachSystem (system:
        let
          overlays = [ (import rust-overlay) ];
          pkgs = import nixpkgs { inherit system overlays; };

          # Stable Rust with the components needed for daily dev work.
          rust = pkgs.rust-bin.stable.latest.default.override {
            extensions = [
              "rust-src"
              "clippy"
              "rustfmt"
              "rust-analyzer"
            ];
          };

          # System libraries required at link time by eframe (X11 + Wayland +
          # OpenGL) and by the czmq/pzmq bindings for the ZMQ ingest layer.
          nativeBuildInputs = with pkgs; [
            rust
            pkg-config
          ];

          buildInputs = with pkgs; [
            # eframe / egui GPU backend
            libGL
            # X11 window backend
            xorg.libX11
            xorg.libXcursor
            xorg.libXrandr
            xorg.libXi
            xorg.libXext
            # Wayland window backend (eframe supports both)
            wayland
            libxkbcommon
            # ZMQ (ingest plan — zmq crate links against libzmq)
            zeromq
            # protobuf compiler (prost-reflect needs protoc for .proto parsing)
            protobuf
            # TLS / crypto (transitive dep of several crates)
            openssl
            mqttx
          ];

          # Paths the dynamic linker must find at `cargo run` time.
          ldLibraryPath = pkgs.lib.makeLibraryPath (buildInputs ++ [ pkgs.libGL ]);
        in
        {
          default = pkgs.mkShell {
            inherit nativeBuildInputs buildInputs;

            shellHook = ''
              export LD_LIBRARY_PATH="${ldLibraryPath}:$LD_LIBRARY_PATH"
              export RUST_BACKTRACE=1
              echo "datavis dev shell — $(rustc --version)"
            '';
          };
        }
      );
    };
}
