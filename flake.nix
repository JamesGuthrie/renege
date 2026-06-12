{
    description = "renege — menu-bar USB negotiation monitor";

    inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";

    outputs = { self, nixpkgs }:
      let
        systems = ["aarch64-darwin" "x86_64-darwin"];
        forAllSystems = nixpkgs.lib.genAttrs systems;
      in {
        overlays.default = final: prev: {
          renege = final.rustPlatform.buildRustPackage {
            pname = "renege";
            version = "0.1.0";
            src = ./.;
            cargoLock.lockFile = ./Cargo.lock;
            buildInputs = [ final.apple-sdk ];
          };
        };
        packages = forAllSystems (system:
          let pkgs = import nixpkgs { inherit system; overlays = [ self.overlays.default ]; };
          in { default = pkgs.renege; });
      };
}
