use std::path::PathBuf;

use prost::Message;

fn main() {
    // Compile the fixed bridge schema with protox (pure Rust, no protoc).
    let fds = protox::compile(["proto/batch.proto"], ["proto"])
        .expect("compiling proto/batch.proto");

    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());

    // FileDescriptorSet bytes for the MCAP recording header.
    std::fs::write(out.join("batch.fds"), fds.encode_to_vec())
        .expect("writing batch.fds");

    // Generate the prost struct from the same descriptor set (no protoc).
    prost_build::Config::new()
        .compile_fds(fds)
        .expect("prost-build compile_fds");

    println!("cargo:rerun-if-changed=proto/batch.proto");
    println!("cargo:rerun-if-changed=build.rs");
}
