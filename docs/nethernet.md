# Local NetherNet preview

The Bedrock adapter accepts HTTP signaling and WebRTC data channels as well as RakNet.
Both feed the same encrypted Bedrock login, pack negotiation, session handling and authoritative world owner.
No other Minecraft server process is required.

## Configuration

With `bedrock.sessions = true` and a loopback Bedrock bind, NetherNet is enabled when the `nethernet` setting is absent.
Existing local preview configurations therefore gain the transport without being overwritten.
Set `bedrock.nethernet = false` to disable it. Explicit `true` requires enabled Bedrock sessions and a loopback address.
Discovery-only mode does not bind an HTTP listener or create a key.

The Bedrock address uses both UDP (RakNet) and TCP (HTTP signaling), normally `127.0.0.1:19132`.
HTTP signaling remains loopback-only. WebRTC opens an ephemeral UDP media port per peer; IPv4 media binds all local interfaces so clients using interface-bound sockets can reach it. Both candidate destinations and incoming source addresses must match loopback or an IPv4 address owned by this machine, captured from the OS at startup (at most 16 non-loopback addresses). No remote machine is admitted by this preview. IPv6 retains loopback media only.
Java keeps its separately configured TCP listener, normally port 25565.

`bedrock.nethernet_identity_path` defaults to `world/nethernet-identity.pem`, resolved relative to the configuration file.
The server creates its operator key once and reuses it across restarts; invalid existing files are preserved and cause startup to fail.
Treat the PEM as private operator data. Preserve the whole `world` directory when updating; do not post the PEM in logs or issues.
Configuration checks do not create the key. New Unix files use mode 0600; Windows uses the containing directory's ACL.

Minecraft's documented plaintext HTTP flow can show a first-use operator trust prompt.
The server signs each answer with the persistent key. A key change can trigger a fresh prompt.
Actual prompt behavior and official-client entry still require game-client verification.

## Scope and bounds

The implementation uses a vendored, pinned sans-IO NetherNet core with Sculith's own bounded Tokio driver.
The local patch separates SCTP initiation from the negotiated DTLS role; its origin and changes are recorded in [SCULITH_PATCHES.md](../vendor/nethernet/SCULITH_PATCHES.md).
Its unbounded Tokio wrapper is used only by test clients.

- HTTP: 16 concurrent connections, 8 KiB parsing buffer, 64 KiB request body, 12-second request and 15-second connection deadlines, no keep-alive.
- Media: at most 64 concurrent transports, four per source IP, establishment deadline from `login_timeout_secs`.
- Application queues: 16 incoming and 32 outgoing messages per transport, with separate 16 MiB aggregate ingress/egress budgets for this backend.
- NetherNet reliable message limit: 2,550,000 bytes; decompressed Bedrock batch limit: 4 MiB. Remote SCTP message limits are capped at 262,144 bytes, including a remote value of zero (unlimited).
- ICE uses complete offers/answers without STUN or TURN. Numeric candidates must belong to this machine. For unresolved/mDNS host candidates, only their advertised UDP ports (up to eight) are tried on the captured same-machine addresses. No DNS lookup or forwarded-header trust is used. Transient candidate-path ICMP errors do not abort other viable ICE checks; the establishment deadline still applies.

These bounds are not a total process-memory limit or a throughput claim.
Application tasks are cancelled and drained before final world capture, releasing their player leases.
The current Bedrock adapter consumes and sends reliable-channel batches only. The unreliable data channel is established,
but its payloads are not applied to the world; official-client movement on that channel needs separate validation and support.

This backend currently requires loopback HTTP signaling and same-machine media sources. It validates offer fingerprint ownership and expiry with `TokenTrust::Any`;
this does **not** authenticate the signaling token's issuer/signature or establish an online account identity.
The existing inner Bedrock authentication policy still applies. Public NetherNet requires official token trust and further validation.

## Verification and sources

Real TCP tests cover shared status, unsigned/tampered offer rejection, fixed/chunked body limits, signed answers,
persistent operator identity and shutdown port release.
A real HTTP/WebRTC headless client and an encrypted RakNet client log in, spawn in the same owner and receive the same committed block delta.
Executable smoke checks probe HTTP status, unsigned-offer rejection and unchanged keys after restart.
An independent aiortc 1.14.0/OpenSSL probe verifies ICE, DTLS, SCTP, both data channels, the server JWT and fingerprint signatures, and an actual Bedrock NetworkSettings response. It reproduced the former interface-address and SCTP-role failures before the corrections. Run it with development dependencies from `scripts/requirements-webrtc.txt` and `python scripts/webrtc_smoke.py`. These dependencies are not required by the installed server.
These are transport tests, not proof that official Minecraft Windows GDK `1.26.63` can enter or render the world.
Preview 3 reached GET/POST and the user accepted the trust prompt, but failed with `Door / InitialConnection-83`. Official-client entry remains unresolved until a retest succeeds.

- [Mojang NetherNet onboarding guide, 19e25de1](https://github.com/Mojang/bedrock-protocol-docs/blob/19e25de129227373e97dfa340af93ccf12c0feb9/additional_docs/NetherNetOnboardingGuide.md), retrieved 2026-11-16 KST: status/join endpoints, full ICE, framing and required signed server identity.
- [NetherNet source ed552b0b](https://github.com/bedrock-crustaceans/nethernet/tree/ed552b0b60dc6f8d54e0d7d42a9c66a9b9bdfb99), Apache-2.0: protocol and sans-IO WebRTC implementation. See [third-party notices](../THIRD_PARTY_NOTICES.md).
