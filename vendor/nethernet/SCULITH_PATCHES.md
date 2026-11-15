# Vendored NetherNet core

Upstream: https://github.com/bedrock-crustaceans/nethernet/tree/ed552b0b60dc6f8d54e0d7d42a9c66a9b9bdfb99/nethernet

License: Apache-2.0, retained unmodified in LICENSE. Sculith's root license remains MIT.
The upstream core is vendored to make one protocol interoperability correction reproducible.
The Tokio test-client wrapper remains pinned to the upstream Git revision.

Local changes:

- src/session/mod.rs: select SCTP initiation from the ICE offerer/answerer role, separately from DTLS role negotiation.
  Previously an actpass offer made the answerer a DTLS client and also an SCTP client. An independent aiortc/OpenSSL
  offerer still initiated SCTP; the answerer refused its INIT because it had no SCTP server configuration.
  The answerer's existing bounded fallback for peers that wait for an INIT is retained.
- Cargo.toml: resolve inherited dependency declarations into a standalone workspace, retain the upstream package
  version and add license/repository/provenance metadata for distribution notices.

All other source files are retained from the pinned upstream core. This is a transport library, not another server process.
