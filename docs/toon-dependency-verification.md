# TOON dependency verification (toon-verify bead)

**Date:** 2026-08-14
**Repo:** `Dicklesworthstone/toon_rust` (spec-first Rust port of TOON)
**Verified by:** the toon-verify bead, per `docs/rust-port-plan.md` §3 / §5.4.

## Findings (verified at implementation time)

| Concern | Finding |
|---|---|
| Package name | **`tru`** (`[package] name = "tru"`, version 0.2.2) — NOT `toon`, NOT `toon_rust` |
| Lib name | **`toon`** (`[lib] name = "toon"`) — imports are `use toon::{encode, try_decode};` |
| Binary | `toon` (`[[bin]] name = "toon"`) |
| API | `toon::encode(input: impl Into<JsonValue>, Option<EncodeOptions>) -> String`; `toon::try_decode(input, Option<DecodeOptions>) -> Result<JsonValue, ToonError>`; `EncodeOptions { indent, delimiter, key_folding, flatten_depth, replacer }` |
| `serde_json::Value` → `JsonValue` | Implemented (smoke-verified round-trip) |
| License | **MIT + OpenAI/Anthropic Rider** (Cargo.toml `license = "MIT"` is incomplete; README/LICENSE carry the rider) — confirm acceptance before shipping |
| MSRV | **1.88** (matches our workspace MSRV) |
| Edition | 2024 |
| Spec | TOON spec **v3.0** |
| Pinned commit | `cb256dcf73ab78c248a14f65840a3fa722ec8682` |

## Cargo.toml pin

```toml
[workspace.dependencies]
tru = { git = "https://github.com/Dicklesworthstone/toon_rust", rev = "cb256dcf73ab78c248a14f65840a3fa722ec8682" }
```

`crates/treehouse-core` enables it behind the optional `toon` feature
(`toon = ["dep:tru"]`), so a checkout without network access to toon_rust still
compiles. Verified: `cargo build --workspace` (no toon) and
`cargo build -p treehouse-core --features toon` both succeed; all 84 tests pass
in both configurations.

## Smoke test output

Encoding `{"name":"Alice","age":30,"tags":["rust","toon"]}`:

```
name: Alice
age: 30
tags[2]: rust,toon
```

## Do NOT substitute

- crates.io **`toon-rust`** — a different project (spec v1.4, lib name
  `toon_rust`, MIT-only). Not compatible with the plan's `toon` API surface.
- crates.io **`toon-format`** — another separate project.
