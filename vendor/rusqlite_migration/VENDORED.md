# Vendored: rusqlite_migration 2.5.0

This is the published crates.io source of `rusqlite_migration` 2.5.0, the version tapectl
locked before (`.crate` sha256 `410e4d2d97ff816796ed012b789c7381ae42c09a809822a75d29a01022181184`,
the checksum in tapectl's earlier `Cargo.lock`), Apache-2.0, by Clément Joly and contributors
(https://github.com/cljoly/rusqlite_migration). It is unchanged, with two exceptions:
- `Cargo.toml` asks for `rusqlite` 0.41 rather than 0.39;
- the crate's `Cargo.lock` is removed.

tapectl's `Cargo.toml` uses it through `[patch.crates-io]`, for #514: RUSTSEC-2026-0459 is fixed
only in libsqlite3-sys 0.39 (rusqlite 0.41), and no released rusqlite_migration accepts rusqlite
0.41 yet. Upstream's open PR #370 makes the same change to 2.6.0. 2.6.0's `src/` is
byte-identical to 2.5.0's, but it declares Rust 1.95, above tapectl's 1.94.1.

When a release of rusqlite_migration accepts rusqlite 0.41 and tapectl's Rust, delete this
directory and the `[patch.crates-io]` entry, and ask for that release.
