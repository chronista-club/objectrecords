//! Error type for `objectrecords-db`.

use thiserror::Error;

/// Errors that can arise while moving data between SurrealDB rows and core
/// [`objectrecords_core::Record`] values.
///
/// The variants split into three origins:
///
/// 1. **Schema-level** (`UnknownState`, `UnknownKind`, `EmptyVersionChain`,
///    `MissingContentHash`, `UnexpectedContentHash`) — the row's bytes are
///    syntactically fine but violate an Object Records invariant. These map
///    almost 1:1 to the type-state guarantees of core.
/// 2. **Encoding-level** (`MalformedHash`) — a string field could not be
///    decoded into its core counterpart.
/// 3. **Backend-level** (`Surreal`) — propagation of errors from the
///    `surrealdb` crate. Phase 3.0 does not actually issue any queries, so
///    this variant is only structurally reserved here for the upcoming
///    repository layer (Phase 3.1+).
#[derive(Debug, Error)]
pub enum DbError {
    /// `record.state` was not one of the expected `mutable` / `snapshot` /
    /// `fixed` literals (decision #26).
    #[error("unknown record state: {0:?} (expected mutable | snapshot | fixed)")]
    UnknownState(String),

    /// The persisted state did not match the `Record<S>` the caller asked for.
    ///
    /// E.g., trying to load a row whose `state = "fixed"` into
    /// `Record<Mutable>`.
    #[error("state mismatch: row says {actual:?} but caller requested {requested:?}")]
    StateMismatch {
        /// The state literal stored in the row.
        actual: String,
        /// The state literal expected by the caller.
        requested: &'static str,
    },

    /// `record.kind` did not match any of the built-in [`Kind`] variants and
    /// was not in the `creo:` reserved-prefix form
    /// (`objectrecords_core::RESERVED_CUSTOM_PREFIX`).
    ///
    /// [`Kind`]: objectrecords_core::Kind
    #[error("unknown kind: {0:?}")]
    UnknownKind(String),

    /// A record was reconstructed without any version rows. Empty version
    /// chains are a structural invariant violation
    /// (`Record::<Mutable>::new` always seeds at least one version).
    #[error("empty version chain (record id: {0})")]
    EmptyVersionChain(uuid::Uuid),

    /// `record.state == "fixed"` but `content_hash` was missing
    /// (decision #6 — every `Record<Fixed>` carries its digest).
    #[error("Record<Fixed> is missing content_hash (record id: {0})")]
    MissingContentHash(uuid::Uuid),

    /// `record.state` was `mutable` or `snapshot` but `content_hash` was
    /// present. `Record<Mutable>` / `Record<Snapshot>` are pre-fossilization
    /// (decision #6).
    #[error("Record<{state}> must not have content_hash (record id: {id})")]
    UnexpectedContentHash {
        /// The state literal stored in the row.
        state: &'static str,
        /// The id of the offending record.
        id: uuid::Uuid,
    },

    /// `record.content_hash` could not be decoded into a 32-byte
    /// [`Sha256Hash`].
    ///
    /// The expected format is 64 lowercase hex chars (the same form as
    /// `Sha256Hash`'s `Display` impl).
    ///
    /// [`Sha256Hash`]: objectrecords_core::Sha256Hash
    #[error("malformed sha256 hash: {0:?}")]
    MalformedHash(String),

    /// A `save<S>` was attempted on a record id that already exists in
    /// the backing store. The repository contract is **create-only**
    /// (decision #32, Sub-Q3 = J): updates flow through `add_version`,
    /// not `save`. The variant exists so callers can distinguish "id
    /// collision" from generic backend errors and decide whether to
    /// retry, prompt, or abort.
    #[error("record already exists: {0}")]
    AlreadyExists(uuid::Uuid),

    /// A `save<Snapshot>` was attempted via the generic `save<S>`
    /// entrypoint. The Phase 3.2 contract (decision #37, Sub-Q4 =
    /// F-hybrid) is that snapshots default to **ephemeral** —
    /// in-memory only, never persisted by the create path. Callers
    /// that wish to persist a snapshot must opt in explicitly via
    /// `save_snapshot`, which auto-mints a new DB id and writes a
    /// `snapshot_of` edge back to the source record. This variant
    /// exists so the footgun ("I forgot snapshots have a special
    /// path") surfaces as a clear, redirect-style error rather than
    /// silently saving with the source's id and colliding.
    #[error(
        "save<Snapshot> is not supported; use save_snapshot to opt in to persistence with an independent DB id"
    )]
    SaveSnapshotRequiresExplicitMethod,

    /// Error propagated from the `surrealdb` crate.
    ///
    /// Reserved for the Phase 3.1+ repository layer; Phase 3.0 produces a
    /// `DbError` purely from in-memory data and never instantiates this
    /// variant.
    ///
    /// Boxed because `surrealdb::Error` is ~144 bytes — without the box,
    /// every `Result<T, DbError>` would inflate the success path's stack
    /// footprint (clippy `result_large_err`).
    #[error("surreal backend error: {0}")]
    Surreal(Box<surrealdb::Error>),
}

impl From<surrealdb::Error> for DbError {
    fn from(e: surrealdb::Error) -> Self {
        DbError::Surreal(Box::new(e))
    }
}
