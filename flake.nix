{
  description = "wayland-proxy: A Wayland proxy server with app_id prefixing capabilities";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, rust-overlay }: let 
    
    package = { lib, rustPlatform }:
      rustPlatform.buildRustPackage {
        pname = "wayland-proxy";
        version = "0.1.0";

        src = ./.;

        strictDeps = true;
        cargoLock = {
          lockFile = ./Cargo.lock;
        };
        cargoBuildType = "release";

        meta = with lib; {
          description = "A Wayland proxy server with app_id prefixing capabilities";
          license = licenses.mit;
          platforms = platforms.linux;
        };
      };

    inherit (nixpkgs) lib;
    systems = lib.intersectLists lib.systems.flakeExposed lib.platforms.linux;
    forAllSystems = lib.genAttrs systems;
    nixpkgsFor = forAllSystems (system: nixpkgs.legacyPackages.${system});
  in {
    packages = forAllSystems (
      system: let 
        pkgs = nixpkgsFor.${system};
        rustPlatform = rust-overlay.legacyPackages.${system}.rustPlatform;
        final = pkgs.callPackage package {};
      in
        {
          default = final;
          wayland-proxy = final;
        }
    );
  };
}