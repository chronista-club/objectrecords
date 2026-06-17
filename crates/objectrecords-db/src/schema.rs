//! Hand-written SurrealDB schema for the `objectrecords` namespace.
//!
//! Phase 3 design (decision #29): the schema is held as `const &str`
//! buffers in source rather than as `.surql` migration files or a
//! third-party migration framework. Reasons:
//!
//! 1. Zero added dependencies (decision constitution #1, YAGNI).
//! 2. Schema lives in code — PR diffs show structural change directly.
//! 3. Object Records is immutable-by-design, so schema evolution is
//!    structurally rare.
//! 4. `surrealdb-migrations` etc. can be adopted additively (decision
//!    constitution #3) when sequential ordering becomes complex.
//!
//! Each statement uses the `OVERWRITE` clause so that [`apply`] is
//! idempotent: running it on a fresh namespace creates the tables;
//! running it again on an already-populated namespace updates the
//! definitions in place without dropping data.
//!
//! # Tables
//!
//! - `record` — one row per record id (decision #26 axis B=(ii)
//!   two-table form). Carries `state` / `kind` / `content_hash` /
//!   `owner` / `created_at` / `updated_at`. The application-level UUID
//!   lives in the `id` field; SurrealDB's table-level Thing identifier
//!   is auto-generated and not used by the repository (Phase 3.1
//!   queries by the indexed `id` field).
//! - `version` — one row per [`objectrecords_core::Version`] entry,
//!   FK-linked to its parent record via `record_id`. Versions are
//!   immutable once written (decision #16 — annual rings).
//!
//! # Indexes
//!
//! - `idx_record_id` (UNIQUE on `record.id`) — enforces no duplicate
//!   record UUIDs and turns id-lookups into O(log n) probes. The
//!   UNIQUE violation is what surfaces as
//!   [`crate::DbError::AlreadyExists`] when `save<S>` collides.
//! - `idx_version_id` (UNIQUE on `version.id`) — same role for
//!   versions.
//! - `idx_version_record_id` (NON-UNIQUE on `version.record_id`) —
//!   cheap FK lookup for "fetch all versions of record X". Phase 3.1
//!   `find` issues a `SELECT * FROM version WHERE record_id = $id
//!   ORDER BY created_at` against this index.

use surrealdb::Surreal;
use surrealdb::engine::any::Any;

use crate::error::DbError;

/// `record` table definition (decision #26, #31, **Phase 3.2: owner
/// field removed per decision #33**).
///
/// The application UUID is carried by SurrealDB's auto-managed
/// [`Thing`][surrealdb::sql::Thing] identifier (rows are created as
/// `record:<uuid_str>` from the repository), so no explicit `id`
/// `DEFINE FIELD` is needed — Thing identity is enforced uniquely by
/// SurrealDB itself, which doubles as the [`crate::DbError::AlreadyExists`]
/// signal on collision.
///
/// `state` is a free-form `string`; the application layer
/// ([`crate::PersistedState`]) is the canonical enforcer of the closed
/// `mutable | snapshot | fixed` set. A SurrealDB `ASSERT` clause may be
/// added additively in Phase 3.x once the literal set is fully frozen.
const RECORD_TABLE: &str = "
    DEFINE TABLE OVERWRITE record SCHEMAFULL;
    DEFINE FIELD OVERWRITE state ON record TYPE string;
    DEFINE FIELD OVERWRITE kind ON record TYPE string;
    DEFINE FIELD OVERWRITE content_hash ON record TYPE option<string>;
    DEFINE FIELD OVERWRITE created_at ON record TYPE datetime;
    DEFINE FIELD OVERWRITE updated_at ON record TYPE datetime;
";

/// `version` table definition + indexes (decision #26, #31).
///
/// `body` is `TYPE object` (free-form) because [`crate::PersistedBody`]
/// is a tagged enum — `{ "type": "inline", "bytes": ... }` or
/// `{ "type": "blob_ref", "key": ..., "size": ... }`. SCHEMAFULL fields
/// for each variant could be carved out additively, but a flexible
/// object keeps the schema short while serde does the validation work.
///
/// `record_id` is the FK back to `record.<uuid>` — uuid-typed for the
/// repository's `SELECT * FROM version WHERE record_id = $rid ORDER BY
/// created_at` lookup. The non-unique `idx_version_record_id` index
/// keeps that lookup O(log n).
const VERSION_TABLE: &str = "
    DEFINE TABLE OVERWRITE version SCHEMAFULL;
    DEFINE FIELD OVERWRITE record_id ON version TYPE uuid;
    -- Phase 4.D-fix-2 (2026-05-14): FLEXIBLE allows nested sub-keys
    -- under `body` on a SCHEMAFULL table. PersistedBody serialises via
    -- SurrealValue derive as `{Inline: {bytes: ...}}` or
    -- `{BlobRef: {key: ..., size: ...}}` (externally-tagged enum), so the
    -- variant key is treated as a dynamic sub-field that must not be
    -- rejected by the validator.
    DEFINE FIELD OVERWRITE body ON version TYPE object FLEXIBLE;
    DEFINE FIELD OVERWRITE created_at ON version TYPE datetime;
    DEFINE INDEX OVERWRITE idx_version_record_id ON version FIELDS record_id;
";

/// `created_by` graph edge table (decision #33, Paradigm Ⅱ — Phase
/// 3.2 supersedes the column-based `owner` of Phase 3.1).
///
/// Materialised as `RELATE creo_user:<sub> -> created_by ->
/// record:<uuid> SET at = <ts>`. The edge does not constrain its
/// `FROM` / `TO` types because the `creo_user` table lives in the
/// neighbour creo-memories namespace; cross-namespace strict typing
/// is a Phase 4 concern when the api crate carries the JWT context.
///
/// The `idx_created_by_in` index speeds up the `<-created_by<-creo_user`
/// traversal that powers `list_by_creator`.
const CREATED_BY_EDGE: &str = "
    DEFINE TABLE OVERWRITE created_by TYPE RELATION;
    DEFINE FIELD OVERWRITE at ON created_by TYPE datetime;
    DEFINE INDEX OVERWRITE idx_created_by_in ON created_by FIELDS in;
";

/// `snapshot_of` graph edge table (decision #37, F-hybrid — opt-in
/// Snapshot persistence).
///
/// Materialised as `RELATE record:<new_id> -> snapshot_of ->
/// record:<source_id> SET at = <ts>`. Lets a persisted snapshot carry
/// a back-reference to its source `Mutable` so retrospection queries
/// (e.g., "all snapshots of record X") can traverse `<-snapshot_of`.
const SNAPSHOT_OF_EDGE: &str = "
    DEFINE TABLE OVERWRITE snapshot_of TYPE RELATION;
    DEFINE FIELD OVERWRITE at ON snapshot_of TYPE datetime;
    DEFINE INDEX OVERWRITE idx_snapshot_of_in ON snapshot_of FIELDS in;
";

/// `or_user` table (Phase 4.E Step 5b — JIT provisioning sidecar).
///
/// Identity Option IV: L2 of the
/// 3-layer identity model. Each Auth0 sub gets a UNIQUE OR-internal
/// user id (UUID v7) the moment it first authenticates. The
/// `created_by` graph edge (Phase 3.2 decision #33) continues to
/// reference `creo_user:<sub>` literals — `or_user` is a sidecar
/// lookup table that future Phase 4.F cross-service feature wiring
/// will consume.
///
/// **No ToS / consent gate** (2026-05-14 hearing): OR is internal infrastructure; consent is captured by
/// creo-memories' public surface, not here. JIT provisioning means
/// every freshly-verified Auth0 sub triggers a single
/// `lookup-then-create` cycle in the auth middleware.
///
/// The UNIQUE index on `auth0_sub` is the race protection: parallel
/// first-time requests can both reach the CREATE phase, but the
/// second one fails with `AlreadyExists` and falls back to a SELECT.
const OR_USER_TABLE: &str = "
    DEFINE TABLE OVERWRITE or_user SCHEMAFULL;
    DEFINE FIELD OVERWRITE auth0_sub ON or_user TYPE string;
    DEFINE FIELD OVERWRITE created_at ON or_user TYPE datetime;
    DEFINE INDEX OVERWRITE idx_or_user_auth0_sub ON or_user FIELDS auth0_sub UNIQUE;
";

/// Applies the full schema (record + version + edges + indexes) to
/// `db`.
///
/// Idempotent — safe to call on every connection. The first call
/// against a fresh namespace creates all definitions; subsequent calls
/// update them in place via the `OVERWRITE` clauses.
///
/// # Errors
///
/// Propagates any [`surrealdb::Error`] as
/// [`DbError::Surreal`]. Common failure modes are auth (invalid
/// credentials), connection (broken WS), and ns/db scope (the
/// connection has not yet selected a namespace).
pub async fn apply(db: &Surreal<Any>) -> Result<(), DbError> {
    db.query(RECORD_TABLE).await?;
    db.query(VERSION_TABLE).await?;
    db.query(CREATED_BY_EDGE).await?;
    db.query(SNAPSHOT_OF_EDGE).await?;
    db.query(OR_USER_TABLE).await?;
    Ok(())
}
