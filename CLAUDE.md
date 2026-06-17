# Object Records — Project Guide

**Object Records** is an internal immutable data store: the data plane for logs,
FIX'd documents, datasets, and assets. Mutable interpretation lives elsewhere;
this store holds *records* — immutable and content-addressable.

> Status: design / early implementation. Not yet ready for production use.

## Workspace layout

Cargo workspace, 4 crates (`crates/*`):

| crate | role |
|---|---|
| `objectrecords-core` | domain model — `Record<S>` type-state, `Kind`, `Body`, `Version`. Dependency-free (only `uuid`). |
| `objectrecords-storage` | `BlobStorage` trait + S3-compatible backend (`object_store`). Physical blob layer. |
| `objectrecords-db` | SurrealDB persistence — schema, repository, DTO ⇄ domain conversion. |
| `objectrecords-api` | axum HTTP API — routes, Auth0 JWT verify, error envelope. |

## Build & test

Toolchain is pinned to Rust 1.95 (`rust-version` in `Cargo.toml`).

```sh
cargo check --workspace --all-targets
cargo test --workspace          # Small (unit) tests run by default
cargo clippy --workspace
```

Some integration tests are **env-gated** and skipped unless their backend is
reachable:

- `objectrecords-db` — set `OBJECTRECORDS_SURREAL_TEST_*` to run against a local SurrealDB.
- `objectrecords-storage` — set `AWS_*` / S3 endpoint env to run against a local S3-compatible store.

## Core model

Three-state type-state machine with an "annual rings" version chain and a
"fossilization" metaphor for `fix()`.

```text
  Mutable ──update──▶ Mutable
     │ snapshot (&self, clone)
     ▼                fix(hash)
  Snapshot ─────────────────────▶ Fixed
                                   ▲
  Mutable ───── fix(hash, direct) ─┘
```

- `Record<Mutable>` — default; `update()` appends a `Version`, `snapshot()` clones, `fix()` consumes.
- `Record<Snapshot>` — a camera-shutter copy; `fix()` only (no `update`).
- `Record<Fixed>` — immutable; no mutating methods (enforced at compile time via a sealed `State` trait).

`Kind` is an open enum (`Log` / `Fix` / `Dataset` / `Asset` / `Custom(String)`).
`Body` is either `Inline(Vec<u8>)` (small records) or `BlobRef { key, size }`
(assets stored via `BlobStorage`).

`Record<S>` does not carry an owner — authentication/authorization is handled at
the API layer (Auth0 JWT) and ownership is modeled in the DB as a graph relation.

## Design principles

1. **YAGNI** — start minimal.
2. **Relax YAGNI when reversal is expensive** — open enums, abstraction layers.
3. **Additive-only** — extend rather than replace.
4. **Separation of concerns** — `core` is dependency-free; storage and identity
   are isolated behind traits / the API layer.
5. **Structural necessity beats YAGNI** — e.g. a Fixed asset's `content_hash` is required.

## Related

- README.md — project overview, brand, and links to related repositories.
