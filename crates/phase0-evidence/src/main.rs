use phase0_evidence::render_embedded;

fn main() {
    match render_embedded() {
        Ok(manifest) => {
            println!("{manifest}");
        }
        Err(error) => {
            eprintln!("phase0-evidence: {error}");
            std::process::exit(1);
        }
    }
}
