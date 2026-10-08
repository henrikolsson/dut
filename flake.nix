{
  description = "dut - fast TUI disk usage analyzer";
  # Prebuilt outputs from CI. Nix asks before trusting these unless you're a
  # trusted user or accept-flake-config is set.
  nixConfig = {
    extra-substituters = [ "https://dut.cachix.org" ];
    extra-trusted-public-keys = [ "dut.cachix.org-1:7ztziGz6lkWtRGVps9uqE+0HGD3z+A1TymVxbd7wnLY=" ];
  };
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };
  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
      in
      {
        devShells.default = pkgs.mkShell {
          packages = [
            pkgs.cargo
            pkgs.rustc
            pkgs.clippy
            pkgs.rustfmt
            pkgs.rust-analyzer
          ];
        };
        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "dut";
          version = (pkgs.lib.importTOML ./Cargo.toml).package.version;
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          meta = {
            description = "Fast TUI disk usage analyzer that remembers previous scans";
            homepage = "https://github.com/henrikolsson/dut";
            license = with pkgs.lib.licenses; [
              mit
              asl20
            ];
            mainProgram = "dut";
          };
        };
      }
    );
}
