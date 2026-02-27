@0x9b65e8b550d8a8c9;

using Rust = import "rust.capnp";

struct FileReq {
  transferId @0 :UInt32;
  totalLen @1 :UInt32;
  # Opaque metadata bytes supplied by the sender (e.g. filename, MIME type, etc).
  #
  # This bundle does not interpret these bytes; they are provided for downstream consumers.
  fileMetadata @2 :Data;

  # Maximum chunk payload size (bytes) the sender is able/willing to emit for this transfer.
  # The receiver may respond with a smaller chunk size in `FileAck`.
  senderMaxChunkSize @3 :UInt32;

  # Algorithm used for the file hash.
  fileHashAlgo @4 :FileHashAlgo;

  # Digest of the full file payload.
  fileHash @5 :Data;
}

enum FileHashAlgo {
  sha256 @0;
}

struct FileChunk {
  transferId @0 :UInt32;
  offset @1 :UInt32;
  data @2 :Data;
}

enum FileAckKind {
  accept @0;
  ack @1;
  reject @2;
  complete @3;
  abort @4;
}

enum FileAckError {
  none @0;
  invalidRequest @1;
  chunkSizeTooLarge @2;
  chunkSizeTooSmall @3;
  hashMissing @4;
  hashInvalidLength @5;
  hashAlgoUnsupported @6;
  hashMismatch @7;
}

struct FileAck {
  transferId @0 :UInt32;
  kind @1 :FileAckKind;

  # Cumulative progress: the receiver has accepted all bytes in [0, nextOffset).
  nextOffset @2 :UInt32;

  # Negotiated maximum chunk payload size (bytes). Only meaningful for `kind=accept`.
  chunkSize @3 :UInt32;

  # Only meaningful for `kind=reject`/`kind=abort`.
  error @4 :FileAckError;
}
