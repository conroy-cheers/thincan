# thincan

An application-layer comms protocol, intended to be used on top of CAN transport(s) such as ISO-TP.

## Workspace crates

- `thincan`
  - A tiny application-layer message protocol and router.
  - Defines a simple wire format (message id + body bytes), and provides macros to declare:
    - a bus message registry (*atlas*es)
    - reusable handler groups (*bundle*s)
    - per-device routers (*maplet*s)

- `thincan-file-transfer`
  - A reusable “file transfer” bundle for `thincan`, implemented using Cap’n Proto types.
  - Provides encoding helpers, a state machine for decoding, and a storage trait to persist
    the received bytes.
