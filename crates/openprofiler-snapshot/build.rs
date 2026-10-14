fn main() {
    println!("cargo:rerun-if-changed=proto/snapshot.proto");
    let fds = protox::compile(["proto/snapshot.proto"], ["proto"]).expect("snapshot.proto must compile");
    prost_build::Config::new().compile_fds(fds).expect("prost codegen");
}
