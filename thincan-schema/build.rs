fn main() {
    capnpc::CompilerCommand::new()
        .src_prefix("schema")
        .file("schema/person.capnp")
        .run()
        .expect("schema compiler command");
}
