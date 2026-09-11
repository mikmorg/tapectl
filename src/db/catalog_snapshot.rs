//! Compatibility surface: `db::ontape_catalog` now owns the schema, row
//! types, generation detection, and write/read of the on-tape `catalog.db`.
//! This module stays only because `src/volume/write.rs` and
//! `tests/catalog_rebuild.rs` call `db::catalog_snapshot::build_catalog_snapshot`.

use std::path::Path;

use rusqlite::Connection;

use crate::error::Result;

pub use super::ontape_catalog::*;

/// Build the filtered `catalog.db` for exactly `stage_set_ids` at `out_path`.
/// See `db::ontape_catalog::write` for the implementation.
pub fn build_catalog_snapshot(
    conn: &Connection,
    stage_set_ids: &[i64],
    out_path: &Path,
) -> Result<()> {
    super::ontape_catalog::write(conn, stage_set_ids, out_path)
}
