{
  description = "Finds what on a Mac isn't captured by its declarative config";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

  outputs =
    { self, nixpkgs }:
    let
      forAllSystems = nixpkgs.lib.genAttrs [
        "aarch64-darwin"
        "x86_64-darwin"
        "aarch64-linux"
        "x86_64-linux"
      ];
      cargo = builtins.fromTOML (builtins.readFile ./Cargo.toml);
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.rustPlatform.buildRustPackage {
            pname = "dotfinder";
            inherit (cargo.package) version;
            src = pkgs.lib.cleanSource self;
            cargoLock.lockFile = ./Cargo.lock;
            meta = {
              inherit (cargo.package) description;
              homepage = cargo.package.repository;
              license = pkgs.lib.licenses.mit;
              mainProgram = "dotfinder";
              platforms = pkgs.lib.platforms.unix;
            };
          };
        }
      );

      devShells = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.mkShell {
            packages = with pkgs; [
              cargo
              rustc
              clippy
              rustfmt
              rust-analyzer
            ];
          };
        }
      );

      formatter = forAllSystems (system: nixpkgs.legacyPackages.${system}.nixfmt);
    };
}
