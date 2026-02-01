@0x9b65e8b550d8a8c9;

using Rust = import "rust.capnp";
$Rust.parentModule("thincan_file_transfer");

struct FileReq {
  transferId @0 :UInt32;
  totalLen @1 :UInt32;
}

struct FileChunk {
  transferId @0 :UInt32;
  offset @1 :UInt32;
  data @2 :Data;
}

struct FileAck {
  transferId @0 :UInt32;
}

struct LogLine {
  bytes @0 :Data;
}
