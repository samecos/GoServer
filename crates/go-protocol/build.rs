fn main() -> Result<(), Box<dyn std::error::Error>> {
    let protoc = protoc_bin_vendored::protoc_bin_path()?;
    let mut config = tonic_prost_build::Config::new();
    config.protoc_executable(protoc);
    // The RPC is named Connect; suppress the similarly named client transport helper.
    tonic_prost_build::configure()
        .build_transport(false)
        .compile_with_config(config, &["../../proto/worker.proto"], &["../../proto"])?;
    println!("cargo:rerun-if-changed=../../proto/worker.proto");
    Ok(())
}
