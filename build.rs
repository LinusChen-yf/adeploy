fn main() -> Result<(), Box<dyn std::error::Error>> {
  tonic_prost_build::configure()
    // The upload enum pairs a description of the deployment with a slice of
    // archive, so its variants differ in size by design. A chunk is a megabyte;
    // carrying the larger variant's footprint alongside it costs nothing, and
    // boxing the description would add an allocation to every message to avoid
    // a few hundred bytes on one of them.
    .type_attribute(
      "adeploy.DeployChunk.payload",
      "#[allow(clippy::large_enum_variant)]",
    )
    .compile_protos(&["proto/adeploy.proto"], &["proto"])?;
  Ok(())
}
