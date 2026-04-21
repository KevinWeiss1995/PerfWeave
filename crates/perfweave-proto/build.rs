fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto = "../../proto/perfweave.proto";
    println!("cargo:rerun-if-changed={proto}");
    tonic_build::configure()
        .build_client(true)
        .build_server(true)
        .compile(&[proto], &["../../proto"])?;
    Ok(())
}
