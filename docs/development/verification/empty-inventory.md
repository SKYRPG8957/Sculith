# Empty inventory startup

Source `98245d6aa60e1e913cb70777bd1368c042fa62d1` removes the prototype
starting items. Local validation passes 221 project tests, Clippy, ownership
doctests and the independent Java/native inventory, peer and content checks.
The [Linux workflow](https://github.com/SKYRPG8957/Sculith/actions/runs/43933997477)
completes successfully. Every build/runtime/interoperability step in the
[native Windows run](https://github.com/SKYRPG8957/Sculith/actions/runs/43933997533)
passes, including its static-CRT release executable.

## Stock Java 26.3

A separate fresh offline profile joins by address, accepts the standard required
pack prompt, downloads the converted pack through the configured HTTP proxy and
enters the world with nine empty hotbar slots and an empty hand. The common
static model remains visible. No client mod or injected protocol input is used
for this graphical session.

The graphical test searches for Stone in the creative inventory and puts one
item in the first hotbar slot. A separate byte-level connection observes the
server-owned peer equipment as `minecraft:stone`, count **1**; the observer's own
inventory starts empty. This checks the owner state as well as the game's image.

- Unmodified client JAR SHA-1: `e877b6a07acd633fb3bb475002175cec036e7b87`.
- Server executable SHA-256: `2ed19f36b0139798eb73c6dcdc3e152134a5a8507a54ed8090c8f2c14836c7d8`.
- Downloaded pack: 11,911 bytes, SHA-1 `4ec0f94603861aeceb633e2384b12d7cc3213135`.
- Empty-hotbar capture SHA-256: `020744c227c505b511ca98c0f900893aae6fe1dfe8bfe8e69f131ac2eeb87bb9`.
- Selected-Stone/model capture SHA-256: `d61ddfc4e78272b87ecb93fc59ea95164494dceb47f446d1230ba2112f47bfbe`.

Private logs, downloaded game assets and unedited captures stay outside the
repository. All three owned server/client/display processes are stopped. This
is Linux offline/local evidence; successful real-account sign-in, official
Windows GDK gameplay and Internet/NAT deployment remain separate verification.
