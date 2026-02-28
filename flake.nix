{
  description = "remtodo — keep Apple Reminders and todo.txt in sync";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

  outputs = { self, nixpkgs }:
    let
      devShell = pkgs: pkgs.mkShell {
        packages = with pkgs; [
          cargo
          rustc
          clippy
          rustfmt
          pre-commit
        ];

        shellHook = ''
          if [ ! -f .git/hooks/pre-commit ]; then
            pre-commit install
          fi

          # Swift CLI must be built outside nix develop (system Swift toolchain)
          if [ ! -f swift/.build/release/reminders-helper ] && \
             [ ! -f swift/.build/debug/reminders-helper ]; then
            echo "⚠ Swift CLI not built. Run outside nix develop:"
            echo "  cd swift && swift build -c release"
          fi
        '';
      };
    in {
      devShells.aarch64-darwin.default = devShell nixpkgs.legacyPackages.aarch64-darwin;
      devShells.x86_64-darwin.default  = devShell nixpkgs.legacyPackages.x86_64-darwin;
    };
}
