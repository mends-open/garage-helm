{
  system ? builtins.currentSystem,
}:

with import ./common.nix;

let
  platforms = [
    "x86_64-unknown-linux-musl"
    "i686-unknown-linux-musl"
    "aarch64-unknown-linux-musl"
    "armv6l-unknown-linux-musleabihf"
  ];
  pkgsList = builtins.map (target: import pkgsSrc { 
    inherit system; 
    crossSystem = { config = target; };
  }) platforms;
  pkgsHost = import pkgsSrc {};
  lib = pkgsHost.lib;
  kaniko = (import ./kaniko.nix) pkgsHost;
in 
  lib.flatten (builtins.map (pkgs: [
     pkgs.rustPlatform.rust.rustc
     pkgs.rustPlatform.rust.cargo
     pkgs.buildPackages.stdenv.cc
  ]) pkgsList) ++ [
    kaniko
  ]

