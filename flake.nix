{
    inputs = {
        nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
        flake-utils.url = "github:numtide/flake-utils";
        rust-overlay.url = "github:oxalica/rust-overlay";
    };

    outputs = { nixpkgs, flake-utils, rust-overlay, ...}:
        flake-utils.lib.eachDefaultSystem (system:
            let
                overlays = [ rust-overlay.overlays.default ];
                pkgs = import nixpkgs { inherit system overlays; };

                rust = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;

            in {
                devShells.default = pkgs.mkShell {
                    packages = [
                        rust
                        pkgs.cargo-workspaces
                        pkgs.prek
                        pkgs.nodejs
                        pkgs.commitlint
                    ];
                };
            });
}
