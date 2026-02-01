@0x9b51c2d1ff8ef8d6;

using Rust = import "rust.capnp";
$Rust.parentModule("thincan");

struct Person {
  name @0 :Text;
  email @1 :Text;
}
