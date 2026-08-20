{
  description = "Computer use for Hyprland — an MCP server giving AI agents eyes and hands on your Wayland desktop";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAllSystems (pkgs: rec {
        hyprhands = pkgs.rustPlatform.buildRustPackage {
          pname = "hyprhands";
          version = "0.1.0";
          src = self;
          cargoLock.lockFile = ./Cargo.lock;

          nativeBuildInputs = [ pkgs.makeWrapper ];

          # Runtime tools ride along so `nix run` needs zero setup. Wrapped as
          # a PATH *suffix*: a tool already on the user's PATH wins, and
          # hyprctl is deliberately NOT included — it must come from the
          # running Hyprland session so versions can't skew.
          postInstall = ''
            wrapProgram $out/bin/hyprhands \
              --suffix PATH : ${
                pkgs.lib.makeBinPath (
                  with pkgs;
                  [
                    grim
                    wlrctl
                    wtype
                  ]
                )
              }
          '';

          meta = {
            description = "Computer-use executor for Hyprland, exposed over MCP";
            homepage = "https://github.com/berker-z/hyprhands";
            license = pkgs.lib.licenses.mit;
            mainProgram = "hyprhands";
            platforms = systems;
          };
        };
        default = hyprhands;
      });

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            cargo
            rustc
            clippy
            rustfmt
            rust-analyzer
            # runtime deps, so `cargo run` behaves like the packaged binary
            grim
            wlrctl
            wtype
          ];
        };
      });
    };
}
