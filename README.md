# thincan

*Mostly* thin and lightweight application-layer protocol for transmitting structured data.

Designed initially as a simpler alternative to CANOpen; however, it may be useful for
use over other transports.

## Protocol

- We use Flatbuffers for encoding. RPC is not supported.
- Each stream should be of a known type.
