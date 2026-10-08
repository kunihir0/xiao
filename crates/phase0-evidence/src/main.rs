use phase0_evidence::{EvidenceError, MAX_INPUT_BYTES, render, render_embedded};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process;

fn main() {
    let args: Vec<String> = env::args().collect();
    let result = match args.len() {
        1 => render_embedded(),
        2 if args[1] == "--help" || args[1] == "-h" => {
            println!("Usage: phase0-evidence [--input <PATH>]");
            return;
        }
        3 if args[1] == "--input" => read_file_and_render(&args[2]),
        _ => {
            eprintln!("Usage: phase0-evidence [--input <PATH>]");
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
