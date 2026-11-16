# Local NetherNet preview

The Bedrock adapter accepts HTTP signaling and WebRTC data channels as well as RakNet.
Both feed the same verified Bedrock login, pack negotiation, session handling and authoritative world owner.
RakNet uses the Bedrock application cipher. NetherNet uses its established DTLS protection and sends LoginSuccess directly after verifying login and binding its identity key to the signed SDP key; it does not perform the RakNet application-cipher handshake.
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
The local patch separates SCTP initiation from the negotiated DTLS role; its origin and changes are recorded in [SCULITH_PATCHES.md](../../vendor/nethernet/SCULITH_PATCHES.md).
Its unbounded Tokio wrapper is used only by test clients.

- HTTP: 16 concurrent connections, 8 KiB parsing buffer, 64 KiB request body, 12-second request and 15-second connection deadlines, no keep-alive.
- Media: at most 64 concurrent transports, four per source IP, establishment deadline from `login_timeout_secs`.
- Application queues: 16 incoming and 32 outgoing messages per transport, with separate 16 MiB aggregate ingress/egress budgets for this backend.
- NetherNet reliable message limit: 2,550,000 bytes; decompressed Bedrock batch limit: 4 MiB. Remote SCTP message limits are capped at 262,144 bytes, including a remote value of zero (unlimited).
- ICE uses complete offers/answers without STUN or TURN. Numeric candidates must belong to this machine. For unresolved/mDNS host candidates, only their advertised UDP ports (up to eight) are tried on the captured same-machine addresses. No DNS lookup or forwarded-header trust is used. Transient candidate-path ICMP errors do not abort other viable ICE checks; the establishment deadline still applies.

These bounds are not a total process-memory limit or a throughput claim.
Application tasks are cancelled and drained before final world capture, releasing their player leases.
Both incoming data channels use the same bounded queues and owner validation. Valid unreliable movement commits; invalid movement is corrected over the reliable channel. Movement arriving before the reliable initialization acknowledgement is ignored until the player activates. Outgoing game batches currently use the reliable channel.

This backend still requires loopback HTTP signaling and same-machine media sources.
With `bedrock.require_authentication = true`, startup loads the official Minecraft
signing keys and uses `TokenTrust::Minecraft`: RS256 signature, issuer, audience,
subject, expiry and the detached P-384 fingerprint assertion must all validate.
The subsequent Bedrock login is independently authenticated and its identity key
must match the transport key. Missing keys never select the offline policy.

Key metadata comes only from
`https://authorization.franchise.minecraft-services.net/.well-known/keys`, using
OS certificate trust, no redirects and a 15-second HTTP deadline. Responses are
limited to 64 KiB and 16 entries; supported RSA keys require unique bounded IDs,
2,048–8,192-bit moduli and valid public exponents. Initial failure stops startup.
An owned task refreshes hourly or after an unknown key ID, with at least 60 seconds
between attempts. Failed fetches remain pending. A cache not refreshed for 24 hours
is cleared on its next refresh tick, rejecting new offers until valid keys return.
Two process-wide fetch permits remain held by running blocking HTTP jobs even when
their async waiters are cancelled. Refresh work never runs in the signaling loop.

Explicit local offline mode retains `TokenTrust::Any` for fingerprint ownership
and expiry; it does not establish Microsoft issuer/signature or account identity.
Public NetherNet still needs media endpoint advertisement and remote-peer routing;
these authentication changes do not lift the existing same-machine restriction.

## Verification and sources

Real TCP tests cover shared status, unsigned/tampered offer rejection, fixed/chunked body limits, signed answers,
persistent operator identity and shutdown port release.
A controlled signing fixture additionally exercises real HTTP acceptance, wrong
signature/issuer/audience/subject/expiry/fingerprint rejection, key rotation and
empty-cache rejection without world admission. Cancellation tests retain the fetch
worker bound; clock-driven tests cover retry, refresh and stale-cache decisions.
A live required-authentication startup fetched the actual official keys and rejected
an independently signed non-Microsoft offer with HTTP 401 before media establishment.
This is a negative authority check, not successful real-account login.
Malformed expiry values, including timestamps beyond the platform range or its
clock-skew allowance, are rejected without panicking. The vendor regression
reproduced the original overflow; actual HTTP checks return 401 and keep serving
after those inputs.
Source `a32062f` passes the build and interoperability workflows on
[Linux](https://github.com/SKYRPG8957/Sculith/actions/runs/43912080454) and
[Windows](https://github.com/SKYRPG8957/Sculith/actions/runs/43912080476), including
the transport timestamp regression and Windows static-CRT release executable.
A real HTTP/WebRTC headless client and an encrypted RakNet client log in, spawn in the same owner and receive the same committed block delta.
Executable smoke checks probe HTTP status, unsigned-offer rejection and unchanged keys after restart.
An independent aiortc 1.14.0/OpenSSL probe verifies ICE, DTLS, SCTP, both data channels, the server JWT and fingerprint signatures, network settings and transport-bound Bedrock login proceeding directly to resource-pack negotiation. It reproduced the former interface-address and SCTP-role failures before the corrections. Run it with development dependencies from `tests/interop/requirements.txt` and `python tests/interop/webrtc_smoke.py`. These dependencies are not required by the installed server.
These are transport tests, not proof that official Minecraft Windows GDK `1.26.63` can enter or render the world.
An independent pinned gophertunnel/Go/Pion client also completes signed login, pack negotiation, world spawn and server-side player activation. See `tests/interop/gophertunnel` and `tests/interop/gophertunnel_smoke.py`; this test client is not part of the server executable.

Both independent clients pass against the native Windows Preview 5 release executable in [run 43760016108](https://github.com/SKYRPG8957/Sculith/actions/runs/43760016108), source `3b0113b`. This proves those protocol paths; official Minecraft world entry/rendering remains awaiting a successful retest.
Preview 4 reached WebRTC and protocol 2225 in the official Windows GDK client, then failed at the incorrectly requested application-encryption handshake (`InitialConnection-66`). Official-client entry remains unresolved until a retest succeeds.

Preview 5's official retest completed DTLS-protected login, then failed while loading with `Boat / InitialConnection-148`. The pinned V2225 enum identifies 148 as `MissingStructureData`; Sculith had omitted the four empty structure registries that must precede StartGame.
Preview 6 corrects the sequence, supplies plains/creative definitions and accepts non-mutating loading controls. It logs client-disconnect reasons and packet/violation identifiers without diagnostic strings or credentials. Native Windows [run 43763379458](https://github.com/SKYRPG8957/Sculith/actions/runs/43763379458), source `d4e2d5a`, passes the independent startup-order, full-registry voxel/biome and world-activation checks. Actual Minecraft entry/rendering remains to be retested.

Preview 6's official retest then identified an Interact/33 notification rejected before initialization (`InitialConnection-40` from Sculith's disconnect). Preview 7 discards this notification without creating game intent; bounded ancillary status cannot advance initialization, while repeated login/network/cipher negotiation still fails. Both real transport tests now send the notification before the initialization acknowledgement. Native Windows [run 43766428256](https://github.com/SKYRPG8957/Sculith/actions/runs/43766428256), source `d9b8537`, passes those regressions and both independent probes; official Minecraft entry still needs a successful retest.

- [Mojang NetherNet onboarding guide, 19e25de1](https://github.com/Mojang/bedrock-protocol-docs/blob/19e25de129227373e97dfa340af93ccf12c0feb9/additional_docs/NetherNetOnboardingGuide.md), retrieved 2026-11-16 KST: status/join endpoints, full ICE, framing and required signed server identity.
- [NetherNet source ed552b0b](https://github.com/bedrock-crustaceans/nethernet/tree/ed552b0b60dc6f8d54e0d7d42a9c66a9b9bdfb99), Apache-2.0: protocol and sans-IO WebRTC implementation. See [third-party notices](../../THIRD_PARTY_NOTICES.md).

The transport-specific login behavior and key binding were cross-checked against MIT-licensed [go-nethernet 4c06b550, Conn.DisableEncryption/PublicKey](https://github.com/df-mc/go-nethernet/blob/4c06b550759d8b488ee209eb09ae87cefd6fe849/conn.go) and [gophertunnel e2251a60, login handling](https://github.com/Sandertv/gophertunnel/blob/e2251a60f71d5faa9784d6a4bc817c117cb6af78/minecraft/conn.go). No reference server implementation is linked into Sculith.
