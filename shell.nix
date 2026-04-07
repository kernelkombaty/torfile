{ pkgs ? import <nixpkgs> {} }:

pkgs.mkShell {
  nativeBuildInputs = with pkgs; [
    pkg-config
    rustc
    cargo
  ];

  buildInputs = with pkgs; [
    openssl.dev
    openssl.out
  ];

  OPENSSL_DIR = "${pkgs.openssl.dev}";
  OPENSSL_LIB_DIR = "${pkgs.openssl.out}/lib";
  OPENSSL_INCLUDE_DIR = "${pkgs.openssl.dev}/include";
  PKG_CONFIG_PATH = "${pkgs.openssl.dev}/lib/pkgconfig";
  LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath [ pkgs.openssl.out ];
  RUST_BACKTRACE = "1";
}