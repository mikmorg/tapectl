//! A stand-in for the `age` command line, for the heir's script's tests
//! (`tests/heir_restore.rs`; the plan's T4.10).
//!
//! RESTORE.sh runs `age -d -i KEYFILE [-i KEYFILE ...] < FILE > OUT`. CI has
//! no `age` program, and the tape under test must hold what staging wrote, so
//! this is built from the `age` crate the library itself encrypts with, and
//! takes the arguments the script gives the real one. It says what the real
//! `age` says where the script reads it: `no identity matched any of the
//! file's recipients` when no key opens the file, and any other complaint,
//! after the plaintext so far has been written, when a chunk of the payload
//! does not authenticate. A test fixture: no production command carries it.
//!
//! `heir_age --version` prints a version line, as `age --version` does.

use std::io::{self, Read, Write};
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut identity_files: Vec<String> = Vec::new();
    let mut input: Option<String> = None;
    let mut decrypt = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--version" => {
                println!("heir-age (a stand-in built from the age crate) 1.1.1");
                return ExitCode::SUCCESS;
            }
            "-d" | "--decrypt" => decrypt = true,
            "-i" | "--identity" => match args.next() {
                Some(file) => identity_files.push(file),
                None => return fail("-i needs a file"),
            },
            other if other.starts_with('-') && other != "-" => {
                return fail(&format!("unknown flag {other}"));
            }
            other => input = Some(other.to_string()),
        }
    }
    if !decrypt {
        return fail("only -d is built");
    }
    let mut identities: Vec<age::x25519::Identity> = Vec::new();
    for file in &identity_files {
        let text = match std::fs::read_to_string(file) {
            Ok(t) => t,
            Err(e) => return fail(&format!("failed to read identity file {file}: {e}")),
        };
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            match line.parse::<age::x25519::Identity>() {
                Ok(i) => identities.push(i),
                Err(e) => return fail(&format!("bad identity in {file}: {e}")),
            }
        }
    }
    let reader: Box<dyn Read> = match input.as_deref() {
        None | Some("-") => Box::new(io::stdin().lock()),
        Some(path) => match std::fs::File::open(path) {
            Ok(f) => Box::new(f),
            Err(e) => return fail(&format!("failed to open {path}: {e}")),
        },
    };
    let decryptor = match age::Decryptor::new(reader) {
        Ok(d) => d,
        Err(e) => return fail(&format!("failed to read header: {e}")),
    };
    let mut payload = match decryptor.decrypt(identities.iter().map(|i| i as &dyn age::Identity)) {
        Ok(p) => p,
        Err(age::DecryptError::NoMatchingKeys) => {
            return fail("no identity matched any of the file's recipients");
        }
        Err(e) => return fail(&format!("failed to decrypt: {e}")),
    };
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match payload.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if out.write_all(&buf[..n]).is_err() {
                    return fail("failed to write output");
                }
            }
            Err(e) => {
                let _ = out.flush();
                return fail(&format!(
                    "failed to decrypt and authenticate payload chunk: {e}"
                ));
            }
        }
    }
    if out.flush().is_err() {
        return fail("failed to write output");
    }
    ExitCode::SUCCESS
}

fn fail(message: &str) -> ExitCode {
    eprintln!("age: error: {message}");
    ExitCode::FAILURE
}
