use phase0_evidence::{EvidenceError, MAX_INPUT_BYTES, render, render_embedded};
use std::env;
use std::fs;
use std::io::{self, Read};
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

fn read_file_and_render(path: &str) -> Result<String, EvidenceError> {
    let metadata = fs::metadata(path).map_err(|e| EvidenceError::Io(e.to_string()))?;
    if metadata.len() > MAX_INPUT_BYTES as u64 {
        return Err(EvidenceError::LimitExceeded(format!(
            "input file exceeds {MAX_INPUT_BYTES} bytes"
        )));
    }
    let content = fs::read_to_string(path).map_err(|e| EvidenceError::Io(e.to_string()))?;
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
