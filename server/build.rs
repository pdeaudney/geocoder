//! Proto compilation hook. Runs only when the `grpc` feature is enabled;
//! skips cleanly for feature-free builds so `cargo build` on the
//! reverse-only configuration doesn't force anyone to have `protoc`.

fn main() {
    println!("cargo:rerun-if-changed=proto/geocoder.proto");
    println!("cargo:rerun-if-changed=build.rs");

    // Only generate stubs when the grpc feature is on; otherwise we'd
    // need protoc for no reason.
    if std::env::var_os("CARGO_FEATURE_GRPC").is_none() {
        return;
    }

    tonic_build::configure()
        .build_server(true)
        .build_client(false)
        .compile_protos(&["proto/geocoder.proto"], &["proto"])
        .expect("compile geocoder.proto");
}
