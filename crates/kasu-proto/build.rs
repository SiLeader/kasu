fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto/kasu.proto");
    tonic_prost_build::configure()
        .message_attribute(".", "#[serde(default)]")
        .type_attribute(".", "#[derive(serde::Serialize, serde::Deserialize)]")
        .type_attribute(
            ".kasu.v1.Entry.kind",
            "#[serde(rename_all = \"snake_case\")]",
        )
        .type_attribute(
            ".kasu.v1.DirEntry.kind",
            "#[serde(rename_all = \"snake_case\")]",
        )
        .compile_protos(&["proto/kasu.proto"], &["proto"])?;
    Ok(())
}
