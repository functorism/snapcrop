{ pkgs ? import <nixpkgs> {} }:

pkgs.mkShell {
    nativeBuildInputs = with pkgs.buildPackages; [ cargo rustc rust-analyzer rustfmt ];
    RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
}
