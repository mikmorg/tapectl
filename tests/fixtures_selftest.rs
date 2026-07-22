/// Self-tests for the microcosm fixture generator.
///
/// Validates determinism, size bounds, and media shape conformance.
mod common;

use common::{generate_library, MicroSpec};
use sha2::{Digest, Sha256};
use std::path::Path;

/// Recursively hash all files in a directory tree for comparison.
fn hash_tree(root: &Path) -> String {
    let mut files = Vec::new();

    // Collect all files with their content hashes
    walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_file())
        .for_each(|entry| {
            let path = entry.path();
            let rel_path = path.strip_prefix(root).unwrap().to_string_lossy();
            let content = std::fs::read(path).expect("failed to read file");
            let file_hash = format!("{:x}", Sha256::digest(&content));
            files.push((rel_path.to_string(), file_hash));
        });

    // Sort to ensure deterministic ordering
    files.sort();

    // Hash the manifest
    let mut hasher = Sha256::new();
    for (path, hash) in files {
        hasher.update(path.as_bytes());
        hasher.update(b":");
        hasher.update(hash.as_bytes());
        hasher.update(b"\n");
    }

    format!("{:x}", hasher.finalize())
}

#[test]
fn test_determinism_same_seed_same_tree() {
    let temp1 = tempfile::TempDir::new().expect("failed to create temp dir 1");
    let temp2 = tempfile::TempDir::new().expect("failed to create temp dir 2");

    let spec = MicroSpec {
        n_units: 5,
        seed: 12345,
    };

    let fixtures1 = generate_library(temp1.path(), &spec);
    let fixtures2 = generate_library(temp2.path(), &spec);

    // Both should have 5 units
    assert_eq!(fixtures1.len(), 5);
    assert_eq!(fixtures2.len(), 5);

    // Compare structure
    for (f1, f2) in fixtures1.iter().zip(fixtures2.iter()) {
        assert_eq!(f1.folder_name, f2.folder_name, "folder names must match");
        assert_eq!(f1.total_bytes, f2.total_bytes, "total bytes must match");
        assert_eq!(f1.files.len(), f2.files.len(), "file counts must match");

        for ((name1, size1), (name2, size2)) in f1.files.iter().zip(f2.files.iter()) {
            assert_eq!(name1, name2, "file names must match");
            assert_eq!(size1, size2, "file sizes must match");
        }
    }

    // Compare tree hashes (byte-identical content)
    let hash1 = hash_tree(temp1.path());
    let hash2 = hash_tree(temp2.path());
    assert_eq!(hash1, hash2, "trees must be byte-identical with same seed");
}

#[test]
fn test_determinism_different_seed_different_tree() {
    let temp1 = tempfile::TempDir::new().expect("failed to create temp dir 1");
    let temp2 = tempfile::TempDir::new().expect("failed to create temp dir 2");

    let spec1 = MicroSpec {
        n_units: 5,
        seed: 12345,
    };
    let spec2 = MicroSpec {
        n_units: 5,
        seed: 54321,
    };

    let fixtures1 = generate_library(temp1.path(), &spec1);
    let fixtures2 = generate_library(temp2.path(), &spec2);

    // Compare tree hashes (should be different)
    let hash1 = hash_tree(temp1.path());
    let hash2 = hash_tree(temp2.path());
    assert_ne!(hash1, hash2, "trees must differ with different seeds");

    // At least some units should have different sizes
    let sizes_match = fixtures1
        .iter()
        .zip(fixtures2.iter())
        .all(|(f1, f2)| f1.total_bytes == f2.total_bytes);
    assert!(!sizes_match, "at least some unit sizes should differ");
}

#[test]
fn test_size_bounds() {
    let temp = tempfile::TempDir::new().expect("failed to create temp dir");

    let spec = MicroSpec {
        n_units: 20,
        seed: 99999,
    };

    let fixtures = generate_library(temp.path(), &spec);

    let min_size = 2 * 1024 * 1024; // 2 MiB
    let max_size = 15 * 1024 * 1024; // 15 MiB

    for fixture in &fixtures {
        assert!(
            fixture.total_bytes >= min_size,
            "unit {} size {} too small (min {})",
            fixture.folder_name,
            fixture.total_bytes,
            min_size
        );
        assert!(
            fixture.total_bytes < max_size,
            "unit {} size {} too large (max {})",
            fixture.folder_name,
            fixture.total_bytes,
            max_size
        );
    }
}

#[test]
fn test_dominant_file_share() {
    let temp = tempfile::TempDir::new().expect("failed to create temp dir");

    let spec = MicroSpec {
        n_units: 10,
        seed: 77777,
    };

    let fixtures = generate_library(temp.path(), &spec);

    for fixture in &fixtures {
        if fixture.files.is_empty() {
            continue; // Skip empty units
        }

        // Find the dominant file (largest)
        let dominant_size = fixture.files.iter().map(|(_, size)| size).max().unwrap();
        let ratio = *dominant_size as f64 / fixture.total_bytes as f64;

        assert!(
            (0.85..=0.99).contains(&ratio),
            "unit {} dominant file ratio {} not in range [0.85, 0.99]",
            fixture.folder_name,
            ratio
        );
    }
}

#[test]
fn test_folder_names_sort_deterministically() {
    let temp = tempfile::TempDir::new().expect("failed to create temp dir");

    let spec = MicroSpec {
        n_units: 10,
        seed: 55555,
    };

    let fixtures = generate_library(temp.path(), &spec);

    let names: Vec<_> = fixtures.iter().map(|f| f.folder_name.clone()).collect();
    let mut sorted_names = names.clone();
    sorted_names.sort();

    assert_eq!(
        names, sorted_names,
        "folder names must be generated in sorted order"
    );
}

#[test]
fn test_no_excluded_filenames() {
    let temp = tempfile::TempDir::new().expect("failed to create temp dir");

    let spec = MicroSpec {
        n_units: 10,
        seed: 33333,
    };

    let fixtures = generate_library(temp.path(), &spec);

    let excluded_patterns = ["*.nfo", "*.tmp", "Thumbs.db", ".DS_Store"];

    for fixture in &fixtures {
        for (filename, _) in &fixture.files {
            for pattern in &excluded_patterns {
                if pattern.starts_with("*") {
                    let ext = pattern.strip_prefix("*").unwrap();
                    assert!(
                        !filename.ends_with(ext),
                        "file {} matches excluded pattern {}",
                        filename,
                        pattern
                    );
                } else {
                    assert!(
                        filename != *pattern,
                        "file {} matches excluded pattern {}",
                        filename,
                        pattern
                    );
                }
            }
        }
    }
}

#[test]
fn test_media_shape_sidecars_present() {
    let temp = tempfile::TempDir::new().expect("failed to create temp dir");

    let spec = MicroSpec {
        n_units: 5,
        seed: 11111,
    };

    let fixtures = generate_library(temp.path(), &spec);

    for fixture in &fixtures {
        // Each fixture should have movie.mkv (dominant file)
        let has_mkv = fixture.files.iter().any(|(name, _)| name == "movie.mkv");
        assert!(has_mkv, "unit {} missing movie.mkv", fixture.folder_name);

        // All fixtures should have both sidecars
        let has_jpg = fixture.files.iter().any(|(name, _)| name == "cover.jpg");
        let has_srt = fixture.files.iter().any(|(name, _)| name == "movie.srt");
        assert!(has_jpg, "unit {} missing cover.jpg", fixture.folder_name);
        assert!(has_srt, "unit {} missing movie.srt", fixture.folder_name);
    }
}
