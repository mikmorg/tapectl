/// Microcosm fixture generator for format-v2 testing.
///
/// Scales a 1/1024 deterministic synthetic media collection to exercise real combinatorics
/// without full-scale hardware (per docs/design/v2-open-questions.md §8).
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

/// Microcosm block size — format constant, NEVER scaled.
#[allow(dead_code)]
pub const MICRO_BLOCK: u64 = 524_288;

/// Nominal microcosm tape capacity (÷1024 from production 2400 G).
#[allow(dead_code)]
pub const MICRO_TAPE_NOMINAL: &str = "2400M";

/// Microcosm slice size (÷1024 from production 10 G).
#[allow(dead_code)]
pub const MICRO_SLICE: &str = "10M";

/// Microcosm ENOSPC buffer — NOT ÷1024, a few 512K blocks (8 MiB).
#[allow(dead_code)]
pub const MICRO_ENOSPC: &str = "8M";

/// Specification for deterministic collection generation.
#[derive(Debug, Clone)]
pub struct MicroSpec {
    /// Number of units (folders) to generate.
    pub n_units: usize,
    /// Seed for deterministic randomness; same seed ⇒ byte-identical tree.
    pub seed: u64,
}

/// Metadata describing one generated unit folder.
#[derive(Debug, Clone)]
pub struct UnitFixture {
    /// Folder name at the root (deterministic, sort-ordered).
    pub folder_name: String,
    /// Full path to the generated folder.
    #[allow(dead_code)]
    pub path: PathBuf,
    /// Total bytes in the unit (sum of all files).
    pub total_bytes: u64,
    /// Per-file manifest: (relative path, size in bytes).
    pub files: Vec<(String, u64)>,
}

/// Generate a deterministic microcosm collection under `root`.
///
/// # Determinism
/// Same `root` and `spec` (n_units, seed) produce byte-identical trees;
/// different seed or root produces different content.
///
/// # Derivation scheme
/// - Unit i's size: `2M + (u64::from_le_bytes(sha256(seed || i)[0:8])) % 13M`
///   ⇒ range [2 MiB, 15 MiB)
/// - File content: per 64 KB block, `sha256(seed || i || filename || block_no)`
///   tiled to fill the block (filename in the derivation so a unit's files are
///   distinct streams; one digest per 64 KB keeps debug-mode generation fast)
/// - Media shape: one dominant file (~90% of unit size) + small sidecars
///   (cover.jpg 200 KB, movie.srt 50 KB)
/// - Folder names: zero-padded index + theme (sort deterministically)
///
/// # Returns
/// A vector of UnitFixture describing each generated folder.
///
/// # Panics
/// On I/O errors (permission, disk full, etc.).
pub fn generate_collection(root: &Path, spec: &MicroSpec) -> Vec<UnitFixture> {
    fs::create_dir_all(root).expect("failed to create root directory");

    let mut fixtures = Vec::new();

    for unit_idx in 0..spec.n_units {
        let unit_bytes = compute_unit_size(spec.seed, unit_idx);
        let folder_name = format!("{:04}_{}", unit_idx, "media_unit");

        let unit_path = root.join(&folder_name);
        fs::create_dir_all(&unit_path).expect("failed to create unit directory");

        // Sidecars: small fixed-size files
        let sidecar1_size = 200_000; // 200 KiB for cover
        let sidecar2_size = 50_000; // 50 KiB for subtitle

        let mut files = Vec::new();

        // Main file: movie.mkv (dominant ~90% of unit)
        let main_size = unit_bytes - sidecar1_size - sidecar2_size;
        write_deterministic_file(&unit_path, "movie.mkv", spec.seed, unit_idx, main_size);
        files.push(("movie.mkv".to_string(), main_size));

        // Cover image: cover.jpg (200 KiB)
        write_deterministic_file(&unit_path, "cover.jpg", spec.seed, unit_idx, sidecar1_size);
        files.push(("cover.jpg".to_string(), sidecar1_size));

        // Subtitle file: movie.srt (50 KiB)
        write_deterministic_file(&unit_path, "movie.srt", spec.seed, unit_idx, sidecar2_size);
        files.push(("movie.srt".to_string(), sidecar2_size));

        let total = files.iter().map(|(_, size)| size).sum();

        fixtures.push(UnitFixture {
            folder_name,
            path: unit_path,
            total_bytes: total,
            files,
        });
    }

    fixtures
}

/// Compute the size of unit `idx` given the seed.
/// Range: [2 MiB, 15 MiB)
fn compute_unit_size(seed: u64, unit_idx: usize) -> u64 {
    let mut hasher = Sha256::new();
    hasher.update(seed.to_le_bytes());
    hasher.update((unit_idx as u64).to_le_bytes());
    let result = hasher.finalize();

    let size_bytes = u64::from_le_bytes([
        result[0], result[1], result[2], result[3], result[4], result[5], result[6], result[7],
    ]);

    let min_size = 2 * 1024 * 1024; // 2 MiB
    let range = 13 * 1024 * 1024; // 13 MiB range
    min_size + (size_bytes % range as u64)
}

/// Write a deterministic file with content derived from seed, unit_idx, and block counter.
fn write_deterministic_file(dir: &Path, filename: &str, seed: u64, unit_idx: usize, size: u64) {
    let path = dir.join(filename);
    let file = fs::File::create(&path).expect("failed to create file");
    use std::io::Write;
    let mut writer = std::io::BufWriter::new(file);

    let mut bytes_written: u64 = 0;
    let mut block_counter: u64 = 0;

    while bytes_written < size {
        let block = generate_content_block(seed, unit_idx, filename, block_counter);
        let to_write = std::cmp::min(block.len() as u64, size - bytes_written) as usize;
        writer
            .write_all(&block[..to_write])
            .expect("failed to write file");
        bytes_written += to_write as u64;
        block_counter += 1;
    }

    writer.flush().expect("failed to flush file");
}

/// One 64 KB content block: sha256(seed || unit_idx || filename || block_counter)
/// tiled. One digest per 64 KB (not per 32 bytes) keeps generation fast in
/// debug builds while staying fully deterministic.
fn generate_content_block(
    seed: u64,
    unit_idx: usize,
    filename: &str,
    block_counter: u64,
) -> Vec<u8> {
    const BLOCK: usize = 64 * 1024;
    let mut hasher = Sha256::new();
    hasher.update(seed.to_le_bytes());
    hasher.update((unit_idx as u64).to_le_bytes());
    hasher.update(filename.as_bytes());
    hasher.update(block_counter.to_le_bytes());
    let digest = hasher.finalize();
    let mut block = Vec::with_capacity(BLOCK);
    while block.len() < BLOCK {
        block.extend_from_slice(&digest);
    }
    block.truncate(BLOCK);
    block
}
