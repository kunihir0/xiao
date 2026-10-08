use phase0_evidence::{EvidenceError, MAX_INPUT_BYTES, render, render_embedded};
use std::env;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process;

fn main() {
    let args: Vec<String> = env::args().collect();
    let result = match args.len() {
        1 => render_embedded(),
        2 if args[1] == "--help" || args[1] == "-h" => {
            println!("Usage: phase0-evidence [--input <PATH> | -]");
            return;
        }
        2 if args[1] == "-" => read_stdin_and_render(),
        3 if args[1] == "--input" => read_file_and_render(&args[2]),
        _ => {
            eprintln!("Usage: phase0-evidence [--input <PATH> | -]");
            process::exit(2);
        }
    };

    match result {
        Ok(manifest) => {
            println!("{manifest}");
        }
        Err(err) => {
            eprintln!("phase0-evidence error: {err}");
            process::exit(1);
        }
    }
}

fn validate_safe_input_path(path_str: &str) -> Result<PathBuf, EvidenceError> {
    let lower = path_str.to_ascii_lowercase();
    const BLOCKED_SUBSTRINGS: &[&str] = &[
        "keyring",
        "secret",
        ".env",
        "token",
        "id_rsa",
        "credential",
        "passwd",
        "shadow",
        "private",
    ];

    for blocked in BLOCKED_SUBSTRINGS {
        if lower.contains(blocked) {
            return Err(EvidenceError::Blocked(
                "access to sensitive or protected path is forbidden".to_owned(),
            ));
        }
    }

    let path = Path::new(path_str);

    // Reject symlinks
    let sym_meta = fs::symlink_metadata(path).map_err(|e| EvidenceError::Io(e.to_string()))?;
    if sym_meta.file_type().is_symlink() {
        return Err(EvidenceError::Blocked("symlinks are forbidden".to_owned()));
    }

    if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
        return Err(EvidenceError::Blocked(
            "input must be a .json file".to_owned(),
        ));
    }

    let metadata = fs::metadata(path).map_err(|e| EvidenceError::Io(e.to_string()))?;
    if metadata.len() > MAX_INPUT_BYTES as u64 {
        return Err(EvidenceError::LimitExceeded(format!(
            "input file exceeds {MAX_INPUT_BYTES} bytes"
        )));
    }

    let canonical = path
        .canonicalize()
        .map_err(|e| EvidenceError::Io(e.to_string()))?;

    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let allowed_fixtures_dir = manifest_dir.join("fixtures").canonicalize().ok();
    let target_tmp = manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("target").join("tmp"))
        .and_then(|p| p.canonicalize().ok());

    let is_contained = allowed_fixtures_dir
        .as_ref()
        .is_some_and(|dir| canonical.starts_with(dir))
        || target_tmp
            .as_ref()
            .is_some_and(|dir| canonical.starts_with(dir));

    if !is_contained {
        return Err(EvidenceError::Blocked(
            "input file must be within the crate fixtures directory or temporary test directory"
                .to_owned(),
        ));
    }

    let canonical_str = canonical.to_string_lossy().to_ascii_lowercase();
    for blocked in BLOCKED_SUBSTRINGS {
        if canonical_str.contains(blocked) {
            return Err(EvidenceError::Blocked(
                "canonical path contains sensitive components".to_owned(),
            ));
        }
    }

    Ok(canonical)
}

fn read_file_and_render(path_str: &str) -> Result<String, EvidenceError> {
    let canonical = validate_safe_input_path(path_str)?;
    let content = fs::read_to_string(canonical).map_err(|e| EvidenceError::Io(e.to_string()))?;
    render(&content)
}

fn read_stdin_and_render() -> Result<String, EvidenceError> {
    let mut buffer = Vec::new();
    let mut take = io::stdin().take((MAX_INPUT_BYTES + 1) as u64);
    take.read_to_end(&mut buffer)
        .map_err(|e| EvidenceError::Io(e.to_string()))?;

    if buffer.len() > MAX_INPUT_BYTES {
        return Err(EvidenceError::LimitExceeded(format!(
            "stdin input exceeds {MAX_INPUT_BYTES} bytes"
        )));
    }

    let content = String::from_utf8(buffer).map_err(|e| EvidenceError::Malformed(e.to_string()))?;
    render(&content)
}
