fn main() {
    println!("cargo:rerun-if-changed=proto");
    // protox compiles the schema in pure Rust, so building needs no `protoc`.
    let fds = protox::compile(["proto/agent_link.proto"], ["proto"]).expect("compile agent_link.proto");
    prost_build::Config::new().compile_fds(fds).expect("generate agent_link.proto bindings");
}
