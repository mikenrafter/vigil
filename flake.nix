{
  # vigil — MCP server that keeps an agent's turn open while it watches
  # builds, logs, commands, ports, processes and URLs (see SPEC.md).
  #
  # Facets: `scout` ships the `vigil` binary to every host that keeps this
  # module. There is no flakelet: `vigil mcp` is started by the agent harness,
  # and its state lives in ~/.local/state/vigil. The agent skill is owned by
  # agent-config/skills/vigil and the MCP entry by
  # agent-config/flake.nix.

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
  inputs.nix-scout.url = "github:mikenrafter/nix-scout/feat/host-exclude";
  inputs.nix-scout.inputs.nixpkgs.follows = "nixpkgs";

  outputs = { nixpkgs, ... }@inputs:
  inputs.nix-scout.lib.mkScoutModule ./. inputs {
    scout = { pkgs, system, ... }: {
      packages.${system}.scout = pkgs.rustPlatform.buildRustPackage {
        pname = "vigil";
        version = "0.1.0";
        # Only what cargo needs, so editing SPEC.md or FINDINGS.md does not
        # rebuild the binary.
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
        # sandbox does not allow. Run it with `cargo test` on host-b.
        doCheck = false;
      };
    };
  };
}
