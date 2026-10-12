# On-tape `catalog.db` fixtures from the writers that made them

`gen1.db` and `gen2.db` are operator-envelope `catalog.db` files
(`docs/design/volume-format-v2.md` §1.4) written by the historical writers
themselves, not derived from today's. `db::ontape_catalog`'s test
`files_written_by_the_shipped_writers_read_back_exactly` reads each and pins
every row, so the reader of shipped tapes is held to what those tapes carry.
`gen1.sql` and `gen2.sql` are `sqlite3 .dump`s of the same files, for reading;
the `.db` files are what the test reads.

| File | Generation | Writer | Commit | `PRAGMA user_version` | sha256 |
|---|---|---|---|---|---|
| `gen1.db` | 1 | `db::catalog_snapshot::build_catalog_snapshot` | b3fe31ee96ed0dd6c54188b81546963c4700949e (2026-07-31, #83's landing) | 6 | `320e024003d04f02a2d6ffb0b8b37ed412f7d80a865be84049fd70caec186751` |
| `gen2.db` | 2 | `db::ontape_catalog::write` | v1.0.6, 8ca779f0d8d878cb18e491b27da10d05de379a8e | 28 | `dd02a1128320020922da00640ca6b9695eb604bd103443235c7f656bef878cfe` |

Generation 1 was written from b3fe31e until 4292cf9 (2026-09-11); generation 2
from 4292cf9 through every 1.0.x release, and the `files` copy did not change in
that span. Both copy the catalog's own `files.id` verbatim. Master's
post-030 generation-2 writer (0b1e790 until generation 3) numbered `files.id`
from 1 instead, but it is in no release tag and never wrote a production tape;
`ontape_catalog::fixture::downgrade` mirrors that one, not these.

## How they were made

`make_fixture.rs` here is the driver. For each commit:

```sh
git worktree add --detach <dir> <commit>
cp tests/fixtures/ontape_catalog/make_fixture.rs <dir>/examples/make_fixture.rs
# b3fe31e only: swap the writer call for the line marked WRITER (b3fe31e)
cd <dir> && cargo run --example make_fixture -- <out>.db
```

The driver opens a fresh catalog with that commit's own `db::open` (so all of
its migrations), inserts the rows below with explicit ids, and calls that
commit's writer for stage sets 90 and 93. The mtimes are spelled by the same
chrono call the walk used at both commits
(`DateTime::from_timestamp(secs, 0).to_rfc3339()`). The hashes are synthetic
(a byte repeated), not hashes of real content.

## Source rows

- Tenants 1 `operator` (owns no unit, so not copied), 3 `alice`, 5 `bob`.
- Units 12 `photos` (alice) and 14 `ledgers` (bob).
- Snapshots 40 (photos v3) and 47 (ledgers v1) are on the tape; 41 (photos v4)
  is not.
- `files`, in walk order:

  | id | snapshot | path | type | size | sha256 | modified_at |
  |---|---|---|---|---|---|---|
  | 77001 | 40 | `docs` | dir | 0 | NULL | 2026-09-01T12:00:00+00:00 |
  | 77002 | 40 | `docs/a.txt` | regular | 11 | `a1`x32 | 1960-01-01T00:00:00+00:00 |
  | 77003 | 40 | `docs/latest` | symlink to `a.txt` | 5 | NULL | 2026-09-01T12:00:00+00:00 |
  | 77004 | 40 | `docs/pipe` | FIFO | 0 | NULL | 2026-09-01T12:00:00+00:00 |
  | 77005-77009 | 41 | `v4/0`-`v4/4` | regular | 0 | NULL | (not on the tape) |
  | 77010 | 47 | `fy24.csv` | regular | 20 | `f2`x32 | 2026-09-01T12:00:00+00:00 |
  | 77011 | 47 | `notes` | dir | 0 | NULL | 2026-09-01T12:00:00+00:00 |
  | 77012 | 47 | `notes/q1.txt` | regular | 9 | `9e`x32 | 2026-09-01T12:00:00+00:00 |

  Both writers keep only `is_directory` of the type, so the symlink and the
  FIFO read back as regular files with no link target.
- Stage sets 90 (snapshot 40, two slices: 300, 301) and 93 (snapshot 47, one
  slice: 305), each with a `key_fingerprints` receipt and every slice with a
  `sha256_plain`. The catalog at b3fe31e already had both columns; its writer
  did not copy them, which is what makes the file generation 1.
