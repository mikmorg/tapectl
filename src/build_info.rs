//! Which tapectl is this? (ADR-0012, 2026-09-24 amendment, item 1)
//!
//! Two answers, for two audiences:
//!
//! - [`VERSION`] names the BUILD: package version, the commit it was built
//!   from (`git describe --tags --always --dirty`) and the UTC build day.
//!   This is what `tapectl --version` prints and what every journal row's
//!   `tapectl_version` column stores (`tape::mam_journal`,
//!   `tape::log_pages`, `tape::health`), because ADR-0013 §7's "parse it
//!   later" needs to know which build wrote the parsed columns, and the
//!   package version has read `0.1.0` for every commit of this project.
//! - [`PKG_VERSION`] names the RELEASE and is the only one that reaches the
//!   tape. See its doc comment for why.
//!
//! The raw pieces come from `build.rs` (`TAPECTL_GIT_DESCRIBE`,
//! `TAPECTL_BUILD_DATE`); that script never fails the build, so a source
//! tarball builds as `0.1.0 (unknown, 2026-09-24)`.

/// The package version alone, exactly `CARGO_PKG_VERSION`.
///
/// **This is the writer identity on tape, and it stays this way.** The ID
/// thunk's `tapectl_version` and `BuildInputs.tapectl_version` (which
/// `MANIFEST.toml` carries) are written from `env!("CARGO_PKG_VERSION")` in
/// `volume::write`, not from [`VERSION`], for two reasons:
///
/// 1. On-tape bytes are forever (ADR-0007). `tests/on_tape_golden.rs` pins
///    `MANIFEST.toml` byte for byte, and the format documents the field as
///    the writer's version, not its commit. ADR-0012 rules explicitly that
///    build identity changes no on-tape byte: "if File 0 has no
///    writer-version field today, none is added by this ruling".
/// 2. A heir reading the tape with `RESTORE.sh` and no tapectl gains nothing
///    from a commit hash of a repository they may not have; the build
///    identity belongs in the catalog, where the journal rows that need it
///    live.
///
/// `tests::on_tape_writer_string_is_the_package_version` pins the
/// `volume::write` sites to this string.
pub const PKG_VERSION: &str = env!("CARGO_PKG_VERSION");

/// `git describe --tags --always --dirty` at build time, or `unknown`.
pub const GIT_DESCRIBE: &str = env!("TAPECTL_GIT_DESCRIBE");

/// The build day, UTC, as an RFC 3339 full-date (`YYYY-MM-DD`).
pub const BUILD_DATE: &str = env!("TAPECTL_BUILD_DATE");

/// The build identity: `0.1.0 (d3c514e, 2026-09-24)`, or with `-dirty`
/// after the hash when the tree had uncommitted changes to tracked files.
///
/// Used by `tapectl --version` and every journal writer that records
/// `tapectl_version`. Never by anything that writes tape — see
/// [`PKG_VERSION`].
pub const VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("TAPECTL_GIT_DESCRIBE"),
    ", ",
    env!("TAPECTL_BUILD_DATE"),
    ")"
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_starts_with_the_package_version_and_carries_the_describe() {
        assert_eq!(PKG_VERSION, env!("CARGO_PKG_VERSION"));
        assert!(
            VERSION.starts_with(PKG_VERSION),
            "VERSION must lead with the package version: {VERSION:?}"
        );
        assert!(
            !GIT_DESCRIBE.is_empty(),
            "build.rs must always stamp a describe"
        );
        assert!(
            VERSION.contains(GIT_DESCRIBE),
            "VERSION must carry the git describe: {VERSION:?}"
        );
        assert!(
            VERSION.contains(BUILD_DATE),
            "VERSION must carry the build date: {VERSION:?}"
        );
        assert_eq!(
            VERSION,
            format!("{PKG_VERSION} ({GIT_DESCRIBE}, {BUILD_DATE})")
        );
    }

    /// The build date is a real UTC day, no earlier than the day this
    /// landed. Not "today": a test binary built yesterday and re-run today
    /// is still correct about when it was built.
    #[test]
    fn build_date_is_an_rfc3339_full_date() {
        let parsed = chrono::NaiveDate::parse_from_str(BUILD_DATE, "%Y-%m-%d")
            .unwrap_or_else(|e| panic!("TAPECTL_BUILD_DATE {BUILD_DATE:?} is not YYYY-MM-DD: {e}"));
        let floor = chrono::NaiveDate::from_ymd_opt(2026, 9, 24).unwrap();
        assert!(
            parsed >= floor,
            "build date {parsed} predates the build-identity ruling"
        );
        assert_eq!(BUILD_DATE.len(), 10);
    }

    /// The on-tape writer string is still exactly `CARGO_PKG_VERSION`.
    ///
    /// Both `volume::write` sites — the ID thunk (`IdThunkV2Params`) and
    /// `BuildInputs` (which reaches `MANIFEST.toml`) — sit inside functions
    /// that need a database and a drive, and `tests/on_tape_golden.rs` pins
    /// the bytes only from synthetic `0.1.0-test` inputs, so neither can
    /// catch `write.rs` switching to the build identity. This pins the
    /// source text instead. The count of two is the positive control: a
    /// refactor that moved the sites would fail here and be re-pinned
    /// deliberately, rather than the negative assertion below passing on
    /// nothing.
    #[test]
    fn on_tape_writer_string_is_the_package_version() {
        let write_rs = include_str!("volume/write.rs");
        let on_tape_sites = write_rs
            .matches(r#"tapectl_version: env!("CARGO_PKG_VERSION")"#)
            .count();
        assert_eq!(
            on_tape_sites, 2,
            "expected the ID-thunk and BuildInputs writer strings in volume/write.rs to be \
             env!(\"CARGO_PKG_VERSION\") — on-tape bytes must not carry the build identity"
        );
        assert!(
            !write_rs.contains("build_info::VERSION"),
            "volume/write.rs must never write build_info::VERSION to tape (ADR-0012 item 1)"
        );
    }
}
