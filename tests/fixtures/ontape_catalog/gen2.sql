PRAGMA foreign_keys=OFF;
BEGIN TRANSACTION;
CREATE TABLE tenants (
    id          INTEGER PRIMARY KEY,
    name        TEXT NOT NULL
);
INSERT INTO tenants VALUES(3,'alice');
INSERT INTO tenants VALUES(5,'bob');
CREATE TABLE units (
    id          INTEGER PRIMARY KEY,
    uuid        TEXT NOT NULL,
    name        TEXT NOT NULL,
    tenant_id   INTEGER NOT NULL,
    status      TEXT
);
INSERT INTO units VALUES(12,'0190f5a0-0000-7000-8000-00000000000c','photos',3,'active');
INSERT INTO units VALUES(14,'0190f5a0-0000-7000-8000-00000000000e','ledgers',5,'active');
CREATE TABLE snapshots (
    id            INTEGER PRIMARY KEY,
    unit_id       INTEGER NOT NULL,
    version       INTEGER NOT NULL,
    snapshot_type TEXT,
    source_path   TEXT,
    total_size    INTEGER,
    file_count    INTEGER
);
INSERT INTO snapshots VALUES(40,12,3,'full','/archive/photos',11,3);
INSERT INTO snapshots VALUES(47,14,1,'full','/archive/ledgers',29,2);
CREATE TABLE stage_sets (
    id                   INTEGER PRIMARY KEY,
    snapshot_id          INTEGER NOT NULL,
    slice_size           INTEGER,
    num_slices           INTEGER,
    total_dar_size       INTEGER,
    total_encrypted_size INTEGER,
    key_fingerprints     TEXT
);
INSERT INTO stage_sets VALUES(90,40,1048576,2,1500000,1500400,'["age1alice","age1operator","age1escrow"]');
INSERT INTO stage_sets VALUES(93,47,1048576,1,700,900,'["age1bob","age1operator","age1escrow"]');
CREATE TABLE stage_slices (
    id               INTEGER PRIMARY KEY,
    stage_set_id     INTEGER NOT NULL,
    slice_number     INTEGER NOT NULL,
    size_bytes       INTEGER,
    encrypted_bytes  INTEGER,
    sha256_plain     TEXT,
    sha256_encrypted TEXT
);
INSERT INTO stage_slices VALUES(300,90,1,1048576,1048776,'1111111111111111111111111111111111111111111111111111111111111111','c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1');
INSERT INTO stage_slices VALUES(301,90,2,451424,451624,'2222222222222222222222222222222222222222222222222222222222222222','c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2');
INSERT INTO stage_slices VALUES(305,93,1,700,900,'3333333333333333333333333333333333333333333333333333333333333333','c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3');
CREATE TABLE files (
    id           INTEGER PRIMARY KEY,
    snapshot_id  INTEGER NOT NULL,
    path         TEXT NOT NULL,
    size_bytes   INTEGER,
    sha256       TEXT,
    modified_at  TEXT,
    is_directory INTEGER
);
INSERT INTO files VALUES(77001,40,'docs',0,NULL,'2026-09-01T12:00:00+00:00',1);
INSERT INTO files VALUES(77002,40,'docs/a.txt',11,'a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1','1960-01-01T00:00:00+00:00',0);
INSERT INTO files VALUES(77003,40,'docs/latest',5,NULL,'2026-09-01T12:00:00+00:00',0);
INSERT INTO files VALUES(77004,40,'docs/pipe',0,NULL,'2026-09-01T12:00:00+00:00',0);
INSERT INTO files VALUES(77010,47,'fy24.csv',20,'f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2','2026-09-01T12:00:00+00:00',0);
INSERT INTO files VALUES(77011,47,'notes',0,NULL,'2026-09-01T12:00:00+00:00',1);
INSERT INTO files VALUES(77012,47,'notes/q1.txt',9,'9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e','2026-09-01T12:00:00+00:00',0);
COMMIT;
