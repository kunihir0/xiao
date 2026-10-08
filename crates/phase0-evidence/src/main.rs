use phase0_evidence::{EvidenceError, MAX_INPUT_BYTES, render, render_embedded};
use std::env;
use std::io::{self, Read};
use std::process;

fn main() {
    let args: Vec<String> = env::args().collect();
    let result = match args.len() {
        1 => render_embedded(),
        2 if args[1] == "--help" || args[1] == "-h" => {
            println!("Usage: phase0-evidence [-- -]");
            return;
        }
        2 if args[1] == "-" => read_stdin_and_render(),
        _ => {
            eprintln!("Usage: phase0-evidence [-- -]");
            process::exit(2);
        }
    };

    match result {
        Ok(manifest) => println!("{manifest}"),
        Err(err) => {
            eprintln!("phase0-evidence error: {err}");
            process::exit(1);
        }
    }
}

fn read_stdin_and_render() -> Result<String, EvidenceError> {
    let mut buffer = Vec::new();
    io::stdin()
        .take((MAX_INPUT_BYTES + 1) as u64)
        .read_to_end(&mut buffer)
        .map_err(|e| EvidenceError::Io(e.to_string()))?;
    if buffer.len() > MAX_INPUT_BYTES {
        return Err(EvidenceError::LimitExceeded(format!(
            "stdin input exceeds {MAX_INPUT_BYTES} bytes"
        )));
    }
    let content = String::from_utf8(buffer)
        .map_err(|_| EvidenceError::Malformed("stdin is not UTF-8".to_owned()))?;
    render(&content)
}
