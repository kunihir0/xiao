use std::error::Error;

fn main() -> Result<(), Box<dyn Error>> {
    let protoc = protoc_bin_vendored::protoc_bin_path()?;
    prost_build::Config::new()
        .protoc_executable(protoc)
        .compile_protos(
            &[
                "proto/checkin.proto",
                "proto/android_checkin.proto",
                "proto/mcs.proto",
            ],
            &["proto"],
        )?;
    println!("cargo:rerun-if-changed=proto/");
    Ok(())
}
