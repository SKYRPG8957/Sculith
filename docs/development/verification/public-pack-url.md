# Public pack URL verification

Source: [2dca1a2](https://github.com/SKYRPG8957/Sculith/commit/2dca1a2f8e4cff7b25cd1363cf3857ffc133bd71).

The unmodified Java 26.3 game joined through a separate fresh profile, accepted
the ordinary required-pack prompt, downloaded through `/shared/packs/*` on an
independent HTTP proxy and entered the world. The proxy removed `/shared` and
connected to Sculith through a non-loopback interface. The game rendered the
authored checkerboard on held Stone and the Rust model plugin's textured statue.
The gameplay plugin's welcome arrived after player initialization.

- Client JAR SHA-1: `e877b6a07acd633fb3bb475002175cec036e7b87`.
- Server executable SHA-256: `4b79f522fb0bb21c0a6d11a69e610bb1e631c75280f15a8864e92cec962acc01`.
- Fresh game-cache ZIP: 11,911 bytes, SHA-1 `4ec0f94603861aeceb633e2384b12d7cc3213135`, matching the offer and logged proxy GET.
- Direct screenshot SHA-256: `ef8003399a5027d1ab4f021591354bc442e881c1875f7801898bbe69911c757e`.

The private capture retains game/server/proxy records and an unedited screenshot;
no game binaries or build packages are added to the repository. The owned game,
server, proxy and display were stopped after capture. An initial harness config
omitted a disabled listener's required bind and failed validation; the corrected
harness required no engine change.

Local validation passes 204 project tests, Clippy, the architecture check and
the ownership doctest. Independent probes cover both automatic local and explicit
advertised URLs, exact archive bytes, GET/HEAD, method/body/path rejection, delayed
pack consent, native transfer/cache, Java gameplay, WebRTC, forms, sounds, models
and connection observations.

The build and configured interoperability checks also pass on
[Linux](https://github.com/SKYRPG8957/Sculith/actions/runs/43904532834) and
[Windows](https://github.com/SKYRPG8957/Sculith/actions/runs/43904532796), including
the explicit advertised-URL proxy probe and Windows static-CRT release executable.

A same-host reference measured 50 warmed sequential GETs of the same ZIP, each
with a new connection, complete body read and SHA-1 check: direct non-loopback
median 0.199 ms / p95 0.360 ms; Python proxy median 0.769 ms / p95 1.046 ms.
This isolates neither server CPU cost nor Internet, TLS or gameplay throughput.

This verifies offline Java login and HTTP deployment inside the test machine.
It does not establish successful real-account login, Internet routing, production
TLS proxy operation, official Windows Bedrock rendering or arbitrary Bedrock UI
and animation conversion.
