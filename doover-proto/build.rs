fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Prefer an externally provided protoc (PROTOC env), otherwise fall back to
    // the vendored binary so `cargo build` works with no protobuf toolchain.
    if std::env::var_os("PROTOC").is_none() {
        std::env::set_var("PROTOC", protoc_bin_vendored::protoc_bin_path()?);
    }

    tonic_build::configure()
        // clients only: doover-rs writes *client* apps, it does not serve the
        // device_agent service (that is the agent's job).
        .build_client(true)
        .build_server(false)
        .compile_protos(
            &["proto/device_agent.proto", "proto/platform_iface.proto"],
            &["proto"],
        )?;

    println!("cargo:rerun-if-changed=proto/device_agent.proto");
    println!("cargo:rerun-if-changed=proto/platform_iface.proto");
    Ok(())
}
