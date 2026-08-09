# Rust Template Repository

This is a template repository for rust projects. It features the following:

- Rust Toolchain via a [Nix flake](./flake.nix) with [direnv](./.envrc) support
- Commit hooks for formatting, linting, and conventional commits via
  [prek](prek.toml)
- Opinionated [linting configuration](Cargo.toml)
- [GPL](LICENSE-GPL) and [MIT](LICENSE-MIT) licenses already in-repo

## Getting Started

To get started with this template, do the following:

1. Update the `repository` field in [Cargo.toml](./Cargo.toml)
2. Ensure the desired rust version is correct in
   [rust-toolchain.toml](./rust-toolchain.toml)
3. For each crate you add under `crates/`, satisfy `clippy::cargo` by either
   setting `publish = false` (for binaries or internal-only crates) or
   providing all of the following `[package]` fields, since CI runs clippy with
   `-D warnings`:
   - `description`
   - `license` or `license_file`
   - `repository`
   - `readme`
   - `keywords`
   - `categories`

   Shared values (`edition`, `license`, `readme`, `repository`) can be
   inherited from `[workspace.package]` via `field.workspace = true`;
   `description`, `keywords`, and `categories` are usually set per-crate.

## License

This template is licensed under the [MIT License](./LICENSE-MIT).

Any contribution intentionally submitted for inclusion in this repository shall
be licensed as above, without any additional terms or conditions.
