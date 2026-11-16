# Windows

Current builds require Rust and the MSVC linker from Visual Studio Build Tools.
Previous preview downloads have been withdrawn.

## Build and start

From PowerShell:

```powershell
git clone https://github.com/SKYRPG8957/Sculith.git
cd Sculith
cargo build --release --locked
.\target\release\sculith.exe run --config examples\configs\crossplay.toml
```

Add `127.0.0.1:19132` in Bedrock, or `127.0.0.1:25565` in Java 26.3 on the same PC.
This configuration uses local offline identities. Changed blocks save every
60 seconds; Ctrl+C saves again and stops the server. Keep existing configuration
and world files when updating. See [save settings](hosting.md#world-saving).

For Rust plugin examples, build the debug workspace and follow the
[plugin guide](plugins.md). Windows plugin paths need the `.exe` extension.

## Connection problems

Check the first server error and the client version. Only one process can bind
each address/port. Enable transport diagnostics when needed:

```powershell
$env:RUST_LOG='info,sculith::protocol::bedrock=debug,sculith::protocol::java=debug'
.\target\release\sculith.exe run --config examples\configs\crossplay.toml
```

Some UWP installations restrict loopback connections. Check the installed app:

```powershell
Get-AppxPackage *Minecraft* | Select-Object Name, PackageFamilyName
```

If that app requires a loopback exemption, use its actual `PackageFamilyName` in
an administrator PowerShell:

```powershell
CheckNetIsolation.exe LoopbackExempt -a -n="YOUR_PACKAGE_FAMILY_NAME"
```

Use `-d` instead of `-a` to remove it. Desktop installations may not need this.
Loopback settings do not fix unsupported protocol behavior.

Report the Sculith commit, Minecraft version and relevant server error in an
[issue](https://github.com/SKYRPG8957/Sculith/issues). Omit tokens and signed login data.
