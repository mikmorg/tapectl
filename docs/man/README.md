# tapectl man pages

These pages are generated from the clap command definitions in
`src/cli/mod.rs`. They are committed so users and package builders can
read them without needing `clap_mangen` installed.

## View a page without installing

```
man -l docs/man/tapectl.1
man -l docs/man/tapectl-volume.1
```

## Install system-wide

```
sudo install -m 644 docs/man/*.1 /usr/local/share/man/man1/
sudo mandb
```

## Regenerate after changing CLI definitions

```
cargo run --example gen_man
```

This overwrites every file under `docs/man/*.1`. Commit the diff
alongside the CLI change.
