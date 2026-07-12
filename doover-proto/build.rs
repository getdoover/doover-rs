fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Prefer an externally provided protoc (PROTOC env), otherwise fall back to
    // the vendored binary so `cargo build` works with no protobuf toolchain.
    if std::env::var_os("PROTOC").is_none() {
        std::env::set_var("PROTOC", protoc_bin_vendored::protoc_bin_path()?);
    }

    let protos = [
        "proto/device_agent.proto",
        "proto/platform_iface.proto",
        "proto/modbus_iface.proto",
        "proto/health.proto",
    ];

    tonic_build::configure()
        // Apps consume the *client* stubs; the servers are generated too so
        // tests can spin up in-process fakes of the sidecars.
        .build_client(true)
        .build_server(true)
        .compile_protos(&protos, &["proto"])?;

    for proto in protos {
        println!("cargo:rerun-if-changed={proto}");
    }
    Ok(())
}
