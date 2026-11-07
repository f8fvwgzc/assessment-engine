//! Compiles `proto/assessment/v1/assessment.proto` into tonic clients and
//! servers, and writes the encoded `FileDescriptorSet` to
//! `$OUT_DIR/assessment_descriptor.bin` for gRPC server reflection.
//!
//! The proto is compiled by `protox`, in Rust, so no `protoc` is needed on
//! build machines.

use std::error::Error;
use std::path::PathBuf;

const PROTOS: [&str; 1] = ["assessment/v1/assessment.proto"];

fn main() -> Result<(), Box<dyn Error>> {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    let root = manifest.join("../../proto");
    for proto in PROTOS {
        println!("cargo:rerun-if-changed={}", root.join(proto).display());
    }

    let descriptors = protox::compile(PROTOS, [&root])?;

    let out = PathBuf::from(std::env::var("OUT_DIR")?);
    let encoded = prost::Message::encode_to_vec(&descriptors);
    std::fs::write(out.join("assessment_descriptor.bin"), encoded)?;

    tonic_prost_build::configure()
        .build_client(true)
        .build_server(true)
        // Deterministic iteration order, matching the core model's BTreeMaps.
        .btree_map(".")
        .compile_fds(descriptors)?;
    Ok(())
}
