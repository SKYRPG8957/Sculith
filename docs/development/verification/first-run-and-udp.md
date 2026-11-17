# First run and UDP candidate failures — 2026-11-17 KST

First-run source: `60af92d81c601cbc4bf0d536862340f9c3234dd2`.
UDP correction: `431a24e74a78067da67aa55fb6f2d8b72164109b`.
The follow-up `95c7b61` checks shim log writes and enables fortified libc during
fixture compilation; it changes no Rust runtime code.

## First run

`sculith` creates `sculith.toml` with authenticated loopback sessions for both
editions, `world/region.sculith` and 60-second autosaving. It loads no plugins or
packs. Creation stages and syncs the complete template before publishing with
no-clobber semantics. Sixteen concurrent creators publish exactly one complete
file. Existing files, invalid TOML, directories and dangling symlinks are preserved.
This config-file test does not authorize concurrent servers sharing a world.

`check` uses the same first-run settings without writing files. An explicitly
missing `--config` fails. Existing discovery-only files retain their behavior.
`run --offline` changes authentication only in memory; existing listener/media
checks still reject unauthenticated game sessions outside loopback.

Debug and release process probes verify independent Java/native entry without
plugins, Java edits, manual saving and restart from the same config. Configuration
and identity-key hashes remain unchanged. No unsolicited welcome, form or pack
appears on Java entry. A separate bare-executable run initializes public keys from
official OIDC endpoints and requires a Java encryption challenge. It does not
verify a successful official-account login.

First-run CI passes on [Linux](https://github.com/SKYRPG8957/Sculith/actions/runs/44013931826)
and [native Windows/static CRT](https://github.com/SKYRPG8957/Sculith/actions/runs/44013931783).

## UDP failure reproduction

The earlier [development-branch run](https://github.com/SKYRPG8957/Sculith/actions/runs/44010428217)
ended immediately after a successful signaling response with
`Operation not permitted (os error 1)`, before ICE connected.
Linux syscall injection reproduces this with the unchanged `60af92d` release
executable: rejecting one owned-interface destination's `sendto` with `EPERM`
ends the transport; the independent client reaches its dial timeout.

The correction defers outbound permission errors during establishment so other
policy-approved candidates can succeed. The peer-address policy, authentication,
budgets and establishment deadline are unchanged. Receive permission errors and
permission failure after transport readiness still terminate the affected session.

The Linux-only fixture compiles a small `LD_PRELOAD` syscall shim in `/tmp`.
It is never linked into the server. Its Go/Pion client explicitly includes a
loopback candidate so a one-interface runner still has a healthy alternative;
ordinary interop probes retain their original client settings.

The first Linux CI attempt for `431a24e` stops while compiling the fixture:
fortified libc marks an ignored `write` result as an error. It has not run that
attempt's network scenario. The fixture now checks the result and explicitly
uses `_FORTIFY_SOURCE=3`; corrected debug and release probes pass locally.

The `431a24e` Windows run separately fails the native block-pick probe's strict
selection-order assertion after successful world entry. The unchanged Rust code
passes that scenario in the `95c7b61` Windows run and 20 fresh-server Linux runs.
This second failure remains unlocalized. The probe retains the same assertions;
its error now includes the requested target/slot and GUI/inventory/equipment
receipt flags to distinguish a recurrence. No ordering fix is claimed.

The corrected `95c7b61` [Linux workflow](https://github.com/SKYRPG8957/Sculith/actions/runs/44016051832)
passes completely, including the fortified fault fixture. The
[native Windows workflow](https://github.com/SKYRPG8957/Sculith/actions/runs/44016051834)
also passes completely, including native build, static-CRT release binaries and
every runtime/interop step. Both runs contain zero uploaded build artifacts.

| Injected condition | Corrected debug/release result |
| --- | --- |
| One non-loopback destination denied | Another candidate connects; native login/spawn and independent decoding of all 2,457,600 visible blocks pass |
| Every IPv4 destination denied | The configured 3-second server establishment deadline ends the attempt before the client's 20-second deadline |
| Active path denied after spawn | Only that native session ends; the connected Java player continues editing the same owner |

These results explain the recorded permission-error failure. Earlier ICE
timeouts without an underlying socket error remain unlocalized. No Internet/NAT,
TURN, official GDK rendering or positive Microsoft-account claim follows from
this fault fixture. No performance improvement is claimed.

## Reproduce

```sh
cargo build --workspace --locked
python3 tests/interop/bootstrap_smoke.py --client target/gophertunnel-probe
# Linux, with cc and iproute2; shim is built outside the repository:
python3 tests/interop/udp_path_smoke.py --client target/gophertunnel-probe
```

The Go client is built from `tests/interop/gophertunnel` using the pinned module
versions. The corrected source passes 286 project tests, ownership
doctests, Clippy and architecture checks. Debug/release bootstrap, fault-injection and
cross-edition equipment probes pass. No package, release or build artifact is
published.
