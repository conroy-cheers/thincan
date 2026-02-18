@0xb4f4e87e3e5a4b61;

using Rust = import "rust.capnp";
$Rust.parentModule("schema");

struct LogMsg {
  text @0 :Text;
}
