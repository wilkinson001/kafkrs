fn main() {
    // Use a vendored protoc so the build works without a system protoc install.
    std::env::set_var("PROTOC", protoc_bin_vendored::protoc_bin_path().unwrap());

    let mut config = prost_build::Config::new();
    // Re-run when the proto file changes.
    println!("cargo:rerun-if-changed=proto/wire/v1.proto");
    config
        .compile_protos(&["proto/wire/v1.proto"], &["proto"])
        .expect("prost-build failed to compile wire/v1.proto");
}
