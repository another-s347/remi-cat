use std::{env, path::PathBuf};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("proto");

    let public_api_proto = proto_dir.join("public_api.proto");

    if !public_api_proto.exists() {
        println!(
            "cargo:warning=Public API proto file not found at {}",
            public_api_proto.display()
        );
        return Ok(());
    }

    let protoc_path = protoc_bin_vendored::protoc_bin_path()?;
    unsafe {
        env::set_var("PROTOC", protoc_path);
    }

    // Compile public_api.proto (includes auth, telemetry, and triggers)
    tonic_prost_build::configure()
        .build_client(true)
        .build_server(false)
        .compile_protos(&[public_api_proto.clone()], &[proto_dir.clone()])?;

    println!("cargo:rerun-if-changed={}", proto_dir.display());
    println!("cargo:rerun-if-changed={}", public_api_proto.display());

    Ok(())
}
