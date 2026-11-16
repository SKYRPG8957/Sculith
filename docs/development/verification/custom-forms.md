# Shared input controls

The initial API-v11 implementation is `1386d82`; `b398c7e` corrects the concrete
vanilla Boolean/Slider NBT tags. Local validation passes 244 project tests,
Clippy, ownership doctests and the eight-crate architecture check. Independent
Java/native clients run actual Rust controller and observer processes and verify
all four controls, Unicode values, cancellation and rejection/recipient isolation.

The actual unmodified Java 26.3 game accepts the required shared pack, renders
the existing static model and displays the same four input definitions. Actual
keyboard/mouse input supplies `LiveName`, false, option 1 and slider 5; the Rust
plugin receives those values and the game displays their private result. The
session uses a fresh profile, no mods and no injected game packets.

- Server SHA-256: `036bca9f6f4608be01aafc6d63f0d4f88899b38d3769973dd5ae703bc659ee37`.
- Client JAR SHA-1: `e877b6a07acd633fb3bb475002175cec036e7b87`.
- Downloaded pack: 11,911 bytes, SHA-1 `4ec0f94603861aeceb633e2384b12d7cc3213135`.
- Initial UI capture SHA-256: `1df67a2d2304e6c6b02ddca140a26fdffcd24a3fbd6ea6053006e5ed4101aa1e`.
- Edited UI capture SHA-256: `4d4b334cca86e7dac5ff52a1a0611944255348ce7c9a659544e04b0c61b4d559`.
- Private result/model capture SHA-256: `d89e9aba516b227dc13f2b87c5a8d0e39de28da0afb31929dfd7504c4d87bcec`.

The probe launched from the working tree containing the parser fix, subsequently
committed unchanged in runtime source `b398c7e`; its private provenance record
retains the original checkout hash and that distinction. All three owned
processes are stopped; captures, downloaded assets and logs remain outside Git.
This is local offline Linux evidence. Real-account entry, official Windows GDK
input rendering and acoustic playback are not verified by this capture.

The first string-only response hypothesis passed its synthetic client but failed
the real game. JDK bytecode inspection of the pinned concrete Boolean and Slider
handlers establishes ByteTag and FloatTag overrides, while text/option controls
use StringTag. The corrected probe now sends those concrete types and tests
non-finite/fractional slider replies. This is separate from REA's prior artifact
inventory and official BDS runtime evidence; it is not a new REA decompilation.
