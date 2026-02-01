fn main() {
    capnpc::CompilerCommand::new()
        .src_prefix("schema")
        .file("schema/file_transfer.capnp")
        .run()
        .expect("schema compiler command");
}
