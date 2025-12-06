@0xe5a5cff50c6b2450;

using Rust = import "rust.capnp";
$Rust.parentModule("thincan_schema");

struct Message {
  foo @0 :Text;
  bar @1 :Text;
}
