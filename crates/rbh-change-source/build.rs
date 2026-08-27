fn main() -> Result<(), Box<dyn std::error::Error>> {
    let protoc = protoc_bin_vendored::protoc_bin_path()?;
    // SAFETY: Cargo runs this build script in its own process. Setting PROTOC
    // only selects the vendored compiler for the child code-generation step.
    unsafe { std::env::set_var("PROTOC", protoc) };
    tonic_prost_build::configure()
        .build_client(true)
        .build_server(true)
        .compile_protos(&["proto/juicefs_changelog.proto"], &["proto"])?;
    println!("cargo:rerun-if-changed=proto/juicefs_changelog.proto");
    Ok(())
}
