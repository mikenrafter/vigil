{
  description = "vigil: keep a coding agent's turn open while a cheap judge watches builds, logs, commands, ports, processes and URLs";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAllSystems (pkgs:
        let
          vigil = pkgs.rustPlatform.buildRustPackage {
            pname = "vigil";
            version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;
            # Only what cargo needs, so editing the docs does not rebuild the
            # binary.
            src = pkgs.lib.fileset.toSource {
              root = ./.;
              fileset = pkgs.lib.fileset.unions [
                ./Cargo.toml
                ./Cargo.lock
                ./src
                ./tests
              ];
            };
            cargoLock.lockFile = ./Cargo.lock;
            # The suite spawns processes and binds loopback ports, which the Nix
            # sandbox does not allow. Run it with `cargo test` outside the sandbox.
            doCheck = false;
            meta = {
              description = "MCP server that keeps an agent's turn open while a cheap judge watches things";
              homepage = "https://github.com/mikenrafter/vigil";
              license = pkgs.lib.licenses.mit;
              mainProgram = "vigil";
            };
          };
        in
        {
          inherit vigil;
          default = vigil;
        });

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages = [ pkgs.cargo pkgs.rustc pkgs.clippy pkgs.rustfmt ];
        };
      });
    };
}
