//! [`RecordRepository`] — persistence trait for the Object Records
//! type-state model + the [`SurrealRecordRepository`] implementation.
//!
//! Phase 3.2 (decisions #33–#38) reshapes the trait surface around
//! Paradigm Ⅱ (relation-style ownership) and full-atomic mutating
//! operations:
//!
//! - [`RecordRepository::save`] — create-only insert of a typed
//!   `Record<S>` plus its full version chain **plus the `created_by`
//!   graph edge** that establishes attribution. Atomic via
//!   `BEGIN TRANSACTION; … COMMIT TRANSACTION;`. `Record<Snapshot>` is
//!   rejected ([`DbError::SaveSnapshotRequiresExplicitMethod`]) so the
//!   in-memory id collision (Snapshot shares its source's UUID per
//!   Phase 1.5 decision #17) cannot silently overwrite.
//! - [`RecordRepository::save_snapshot`] — explicit opt-in
//!   persistence for snapshots (decision #37, F-hybrid). Mints a
//!   fresh UUID v7 for the DB row and adds a `snapshot_of` edge back
//!   to the source record. Returns the freshly-allocated DB id.
//! - [`RecordRepository::find`] — state-agnostic read returning
//!   [`RecordWithAttribution`]. The caller pattern-matches on
//!   `record` to recover the type-state and reads `attribution` for
//!   the `created_by` edge metadata.
//! - [`RecordRepository::list_by_creator`] — graph-traversal over
//!   `<-created_by<-creo_user` to enumerate every record a CreoID
//!   user has created.
//! - [`RecordRepository::add_version`] — server-side mints a new
//!   [`VersionId`] (UUID v7), inserts a single `version` row, and
//!   refreshes `record.updated_at`. Atomic.
//! - [`RecordRepository::transition_to_fixed`] — atomically promotes
//!   a `Mutable` or `Snapshot` row to `Fixed`, sets
//!   `content_hash`, and rewrites the trailing `BlobRef.key` to
//!   `/fixed/<hash>` per Phase 1.5 decision #15.
//! - [`RecordRepository::delete`] — cascade-deletes versions, all
//!   incoming/outgoing edges (`created_by`, `snapshot_of`), and the
//!   record itself. Atomic.
//!
//! State-specific convenience finds (`find_mutable` etc.) are
//! deferred to Phase 3.x — they can be added with `default impl`
//! signatures so consumers are not broken (decision constitution #3).

use std::future::Future;

use chrono::{DateTime, Utc};
use objectrecords_core::{
    Body, Fixed, Kind, Mutable, Record, Sha256Hash, Snapshot, State, Version, VersionId,
};
use serde::{Deserialize, Serialize};
use surrealdb::Surreal;
use surrealdb::engine::any::Any;
use surrealdb_types::{RecordId, RecordIdKey, SurrealValue};
use uuid::Uuid;

use crate::client::ObjectRecordsDb;
use crate::convert::{StateLiteral, persisted_row_to_any, record_to_persisted_row};
use crate::error::DbError;
use crate::persisted::{
    PersistedAttribution, PersistedBody, PersistedRecord, PersistedRow, PersistedState,
    PersistedVersion,
};

// =============================================================================
// AnyRecord — state-agnostic return type for find() and list_by_creator()
// =============================================================================

/// Type-state-agnostic envelope for a single record as loaded from
/// SurrealDB. Each variant carries the actual `Record<S>` so the
/// caller can pattern-match and recover the compile-time guarantees.
///
/// This enum lives in the db crate (not in core) — decision #14 keeps
/// core dependency-free.
#[derive(Debug, Clone)]
pub enum AnyRecord {
    /// State `mutable` — accepts further `update` / `snapshot` / `fix`.
    Mutable(Record<Mutable>),
    /// State `snapshot` — accepts `fix` only (decision #17).
    Snapshot(Record<Snapshot>),
    /// State `fixed` — fully fossilized, no mutation.
    Fixed(Record<Fixed>),
}

/// A record as fetched from SurrealDB, paired with its `created_by`
/// edge attribution (decision #35, Phase 3.2). Always emitted together
/// because every record must have a creator (Paradigm Ⅱ — there is no
/// system-owned loophole).
#[derive(Debug, Clone)]
pub struct RecordWithAttribution {
    /// The typed record reconstructed from the wire form.
    pub record: AnyRecord,
    /// The `created_by` edge metadata attached to this record.
    pub attribution: PersistedAttribution,
}

// =============================================================================
// Trait
// =============================================================================

/// Persistence interface for `Record<S>` values.
///
/// Phase 3.1 ships a single implementor ([`SurrealRecordRepository`]).
/// The trait exists so that future backends (SQLite, RocksDB, in-memory
/// for tests) can plug in without breaking call sites.
///
/// Methods return `impl Future<...> + Send` rather than `async fn` —
/// same desugar as [`objectrecords_storage::BlobStorage`] (decision
/// #19), so the futures are forced `Send`-able for axum.
pub trait RecordRepository {
    /// Inserts `record` and its full version chain under `creator`,
    /// atomically writing the `created_by` edge in the same
    /// SurrealDB transaction.
    ///
    /// `creator` should be the CreoID Thing literal
    /// `"creo_user:<sub>"`. The repository splits it into table /
    /// identifier components for the `RELATE` statement.
    ///
    /// Returns:
    /// - [`DbError::AlreadyExists`] when `record.id()` collides with
    ///   an existing row.
    /// - [`DbError::SaveSnapshotRequiresExplicitMethod`] when called
    ///   with a `Record<Snapshot>` — snapshots have a separate
    ///   opt-in path ([`Self::save_snapshot`]).
    fn save<S>(
        &self,
        record: &Record<S>,
        creator: &str,
    ) -> impl Future<Output = Result<(), DbError>> + Send
    where
        S: State + StateLiteral + Sync;

    /// Persists a [`Record<Snapshot>`] under a freshly-minted UUID v7
    /// (decision #37, F-hybrid). Adds a `snapshot_of` edge from the
    /// new DB row back to the source record so that retrospection
    /// is possible. Returns the new DB id (which differs from
    /// `snapshot.id()`, which still carries the source UUID per
    /// Phase 1.5 decision #17).
    fn save_snapshot(
        &self,
        snapshot: &Record<Snapshot>,
        creator: &str,
    ) -> impl Future<Output = Result<Uuid, DbError>> + Send;

    /// Reads the record with `id`, its full version chain, and the
    /// `created_by` edge attribution. Returns `Ok(None)` when no
    /// record matches.
    fn find(
        &self,
        id: Uuid,
    ) -> impl Future<Output = Result<Option<RecordWithAttribution>, DbError>> + Send;

    /// Enumerates every record where `<-created_by<-creo_user`
    /// resolves to `creator`. Phase 3.2 minimum surface for
    /// "show me my records" (decision #36).
    fn list_by_creator(
        &self,
        creator: &str,
    ) -> impl Future<Output = Result<Vec<RecordWithAttribution>, DbError>> + Send;

    /// Appends a new version row to the record at `record_id`.
    /// Atomic: version INSERT and `record.updated_at` UPDATE are in
    /// a single transaction.
    ///
    /// The repository server-side mints the [`VersionId`] (UUID v7)
    /// so no two concurrent appenders collide on a client-side id
    /// source.
    fn add_version(
        &self,
        record_id: Uuid,
        body: Body,
    ) -> impl Future<Output = Result<Version, DbError>> + Send;

    /// Promotes the record at `id` from `Mutable` or `Snapshot` to
    /// `Fixed` (decision #34). Atomically: sets `state = "fixed"`,
    /// records the SHA-256 `hash`, and rewrites the trailing
    /// `Body::BlobRef.key` to `/fixed/<hash>` per Phase 1.5
    /// decision #15.
    fn transition_to_fixed(
        &self,
        id: Uuid,
        hash: Sha256Hash,
    ) -> impl Future<Output = Result<Record<Fixed>, DbError>> + Send;

    /// Cascade-deletes the record, every `version` row whose
    /// `record_id` matches, and the incident `created_by` /
    /// `snapshot_of` edges. Atomic. Returns `true` if a record was
    /// removed.
    fn delete(
        &self,
        id: Uuid,
    ) -> impl Future<Output = Result<bool, DbError>> + Send;
}

// =============================================================================
// Internal SurrealDB I/O types
// =============================================================================

/// Body of a `record` row sans the SurrealDB-managed Thing identity.
///
/// Phase 3.2 (decision #33, Paradigm Ⅱ): the `owner` column has been
/// removed — attribution lives on the `created_by` graph edge handled
/// separately by the repository.
#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
struct RecordRowDb {
    // Phase 4.D-fix-2: stored as a plain string so the SurrealDB schema
    // (`DEFINE FIELD state ... TYPE string`) accepts it. The v3
    // `SurrealValue` derive on the typed `PersistedState` enum would
    // instead emit a `{Mutable: {}}` object (default `VariantKey`
    // strategy), which the schema rejects.
    state: String,
    kind: String,
    content_hash: Option<String>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl RecordRowDb {
    fn from_persisted(p: &PersistedRecord) -> Self {
        Self {
            state: p.state.as_literal().to_string(),
            kind: p.kind.clone(),
            content_hash: p.content_hash.clone(),
            created_at: p.created_at,
            updated_at: p.updated_at,
        }
    }

    fn into_persisted(self, id: Uuid) -> Result<PersistedRecord, DbError> {
        let state = PersistedState::from_literal(&self.state).map_err(DbError::UnknownState)?;
        Ok(PersistedRecord {
            id,
            state,
            kind: self.kind,
            content_hash: self.content_hash,
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}

/// Body of a `version` row sans the SurrealDB-managed Thing identity.
/// Includes [`RecordId`] explicitly so the version UUID can be recovered
/// from the raw identifier on read.
///
/// v3 SDK split: `bind` accepts the [`SurrealValue`]-deriving
/// [`VersionRowWrite`] (no `id` field, stable shape), `take` /
/// `select` deserialise into [`VersionRowRead`] (carries the
/// SurrealDB-assigned [`RecordId`]). v2 used a single struct with
/// `#[serde(skip_serializing)]` to handle both directions, but the
/// SurrealValue trait lives outside serde so the cleanest split is
/// two narrow types.
#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
struct VersionRowWrite {
    record_id: Uuid,
    body: PersistedBody,
    created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
struct VersionRowRead {
    /// SurrealDB Thing identifier (`version:<uuid_str>`). Always present
    /// in SELECT * responses.
    id: RecordId,
    record_id: Uuid,
    body: PersistedBody,
    created_at: DateTime<Utc>,
}

impl VersionRowWrite {
    fn from_persisted(v: &PersistedVersion) -> Self {
        Self {
            record_id: v.record_id,
            body: v.body.clone(),
            created_at: v.created_at,
        }
    }
}

impl VersionRowRead {
    fn into_persisted(self) -> Result<PersistedVersion, DbError> {
        let raw = match self.id.key {
            RecordIdKey::String(s) => s,
            other => {
                return Err(DbError::MalformedHash(format!(
                    "version id is not a string key: {other:?}",
                )));
            }
        };
        let id = Uuid::parse_str(&raw)
            .map_err(|_| DbError::MalformedHash(format!("version id is not a UUID: {raw}")))?;
        Ok(PersistedVersion {
            id,
            record_id: self.record_id,
            body: self.body,
            created_at: self.created_at,
        })
    }
}

/// Phase 4.E Step 5b — OR-internal user identity (Identity Option IV
/// L2). One row per Auth0 sub that has ever authenticated against the
/// OR API. Created lazily by [`SurrealRecordRepository::upsert_or_user`]
/// on first successful JWT verification (JIT provisioning, no consent
/// gate — see internal design notes + 2026-05-14 hearing).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrUser {
    /// OR-internal user id (UUID v7, decision #10 idiom).
    pub id: Uuid,
    /// Auth0 sub claim — the Identity SSOT per Option IV
    ///. UNIQUE-indexed.
    pub auth0_sub: String,
    /// First-authenticate timestamp.
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
struct OrUserRowDb {
    id: RecordId,
    auth0_sub: String,
    created_at: DateTime<Utc>,
}

impl OrUserRowDb {
    fn into_or_user(self) -> Result<OrUser, DbError> {
        let raw = match self.id.key {
            RecordIdKey::String(s) => s,
            other => {
                return Err(DbError::MalformedHash(format!(
                    "or_user id is not a string key: {other:?}",
                )));
            }
        };
        let id = Uuid::parse_str(&raw)
            .map_err(|_| DbError::MalformedHash(format!("or_user id is not a UUID: {raw}")))?;
        Ok(OrUser {
            id,
            auth0_sub: self.auth0_sub,
            created_at: self.created_at,
        })
    }
}

/// Wire-form of a `created_by` edge as projected by SurrealQL.
///
/// `creator` is captured as a string via `type::string(in)` in the
/// SELECT; this preserves the full `creo_user:<sub>` literal that the
/// api layer originally wrote, without the repository needing to know
/// the `creo_user` schema.
#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
struct CreatedByEdgeRow {
    creator: String,
    at: DateTime<Utc>,
}

// =============================================================================
// Helpers
// =============================================================================

/// Splits a Thing literal `"<table>:<identifier>"` into its parts.
///
/// SurrealDB's `type::record(table, identifier)` builder needs the two
/// parts as separate strings; the api layer typically holds the
/// combined literal so the repository accepts that form and splits
/// on the first `:`.
fn parse_thing_literal(s: &str) -> Result<(&str, &str), DbError> {
    s.split_once(':').ok_or_else(|| DbError::MalformedHash(
        format!("expected Thing literal `<table>:<id>`, got {s:?}"),
    ))
}

/// Phase 4.D-fix-2 (2026-05-14): strip the backticks v3's
/// `type::string(<record_id>)` adds around the id portion when it
/// contains chars (hyphens etc.) that need quoting. Input
/// `creo_user:\`abc-123\`` becomes `creo_user:abc-123`.
fn strip_thing_backticks(s: &str) -> String {
    s.replace('`', "")
}

// =============================================================================
// SurrealRecordRepository
// =============================================================================

/// SurrealDB-backed implementation of [`RecordRepository`].
///
/// Cheap to clone — the underlying [`ObjectRecordsDb`] handle is
/// `Arc`-shared, so cloning the repository is cloning a pointer.
#[derive(Debug, Clone)]
pub struct SurrealRecordRepository {
    db: ObjectRecordsDb,
}

impl SurrealRecordRepository {
    /// Wraps the provided [`ObjectRecordsDb`] connection.
    #[must_use]
    pub fn new(db: ObjectRecordsDb) -> Self {
        Self { db }
    }

    fn surreal(&self) -> &Surreal<Any> {
        self.db.inner()
    }

    /// SELECTs the `created_by` edge attribution for a given record id.
    /// Used by [`Self::find`] and [`Self::list_by_creator`] to glue the
    /// edge metadata onto a freshly-fetched record.
    async fn fetch_attribution(
        &self,
        record_id: Uuid,
    ) -> Result<Option<PersistedAttribution>, DbError> {
        let mut response = self
            .surreal()
            .query(
                "SELECT type::string(in) AS creator, at FROM created_by \
                 WHERE out = type::record('record', $rid) LIMIT 1",
            )
            .bind(("rid", record_id.to_string()))
            .await?;
        let edge: Option<CreatedByEdgeRow> = response.take(0)?;
        Ok(edge.map(|e| PersistedAttribution {
            creator: strip_thing_backticks(&e.creator),
            at: e.at,
        }))
    }

    /// SELECTs all version rows for a record and reconstructs the
    /// `PersistedVersion` chain in oldest-first order.
    async fn fetch_versions(
        &self,
        record_id: Uuid,
    ) -> Result<Vec<PersistedVersion>, DbError> {
        let mut response = self
            .surreal()
            .query(
                "SELECT * FROM version WHERE record_id = $rid ORDER BY created_at ASC",
            )
            .bind(("rid", record_id))
            .await?;
        let rows: Vec<VersionRowRead> = response.take(0)?;
        rows.into_iter().map(|r| r.into_persisted()).collect()
    }

    /// Common path: assemble a [`RecordWithAttribution`] given a
    /// freshly-fetched record id (the record body and edge are read
    /// here, the version chain is pulled in-line).
    async fn assemble(&self, id: Uuid) -> Result<Option<RecordWithAttribution>, DbError> {
        let id_str = id.to_string();
        let record_row: Option<RecordRowDb> = self
            .surreal()
            .select(("record", id_str.as_str()))
            .await?;
        let Some(record_row) = record_row else {
            return Ok(None);
        };
        let persisted_record = record_row.into_persisted(id)?;
        let versions = self.fetch_versions(id).await?;
        let attribution = self
            .fetch_attribution(id)
            .await?
            .ok_or_else(|| {
                // Every record must have a `created_by` edge per
                // Paradigm Ⅱ; missing attribution is a structural
                // invariant violation, not a query miss.
                DbError::MalformedHash(format!(
                    "record {id} has no created_by edge — Paradigm Ⅱ invariant violation",
                ))
            })?;
        let row = PersistedRow {
            record: persisted_record,
            versions,
            attribution: attribution.clone(),
        };
        let any = persisted_row_to_any(row)?;
        Ok(Some(RecordWithAttribution {
            record: any,
            attribution,
        }))
    }

    /// Phase 4.E Step 5b — JIT provisioning. Returns the [`OrUser`]
    /// for the given Auth0 sub, creating it on first sight.
    ///
    /// Lookup-then-create with race fallback:
    /// 1. `SELECT * FROM or_user WHERE auth0_sub = $sub LIMIT 1` (the
    ///    hot path — most requests find an existing row).
    /// 2. On miss, `CREATE or_user:<new_uuid_v7> CONTENT {...}`.
    /// 3. If the CREATE collides on the `idx_or_user_auth0_sub` UNIQUE
    ///    index (a parallel first-time request won the race), re-SELECT
    ///    and return the freshly-created peer's row.
    ///
    /// # Errors
    /// Propagates SurrealDB or schema errors. Structural impossibility
    /// (CREATE → UNIQUE conflict → re-SELECT misses) surfaces as
    /// [`DbError::MalformedHash`] — that would mean the row was
    /// deleted between the CREATE attempt and the recovery SELECT.
    pub async fn upsert_or_user(&self, auth0_sub: &str) -> Result<OrUser, DbError> {
        if let Some(existing) = self.find_or_user_by_sub(auth0_sub).await? {
            return Ok(existing);
        }

        let new_id = Uuid::now_v7();
        let now = Utc::now();
        let result = self
            .surreal()
            .query(
                "CREATE type::record('or_user', $id) CONTENT \
                 { auth0_sub: $sub, created_at: $at }",
            )
            .bind(("id", new_id.to_string()))
            .bind(("sub", auth0_sub.to_string()))
            .bind(("at", now))
            .await?
            .check();

        match result {
            Ok(_) => Ok(OrUser {
                id: new_id,
                auth0_sub: auth0_sub.to_string(),
                created_at: now,
            }),
            Err(_) => {
                // Either a genuine error or a UNIQUE-index race; both
                // are recoverable by re-SELECT. If the row is found,
                // return it; otherwise surface as a structural error.
                self.find_or_user_by_sub(auth0_sub).await?.ok_or_else(|| {
                    DbError::MalformedHash(format!(
                        "or_user CREATE failed and re-SELECT for sub {auth0_sub:?} missed — \
                         either a non-race error or the row vanished",
                    ))
                })
            }
        }
    }

    async fn find_or_user_by_sub(&self, auth0_sub: &str) -> Result<Option<OrUser>, DbError> {
        let mut response = self
            .surreal()
            .query("SELECT * FROM or_user WHERE auth0_sub = $sub LIMIT 1")
            .bind(("sub", auth0_sub.to_string()))
            .await?;
        let row: Option<OrUserRowDb> = response.take(0)?;
        row.map(OrUserRowDb::into_or_user).transpose()
    }
}

impl RecordRepository for SurrealRecordRepository {
    async fn save<S>(&self, record: &Record<S>, creator: &str) -> Result<(), DbError>
    where
        S: State + StateLiteral + Sync,
    {
        // Reject Snapshot up-front (decision #37 footgun guard).
        if S::LITERAL == PersistedState::Snapshot {
            return Err(DbError::SaveSnapshotRequiresExplicitMethod);
        }

        let row = record_to_persisted_row(record, creator.to_string());
        let id = row.record.id;
        let id_str = id.to_string();

        // Existence check (Phase 3.1 idiom; race window narrow).
        let existing: Option<RecordRowDb> = self
            .surreal()
            .select(("record", id_str.as_str()))
            .await?;
        if existing.is_some() {
            return Err(DbError::AlreadyExists(id));
        }

        write_record_atomic(self.surreal(), &row).await
    }

    async fn save_snapshot(
        &self,
        snapshot: &Record<Snapshot>,
        creator: &str,
    ) -> Result<Uuid, DbError> {
        // Generate an independent DB id; the in-memory snapshot.id()
        // (== source Mutable's id) is preserved as the `snapshot_of`
        // edge target so retrospection works.
        let new_db_id = Uuid::now_v7();
        let source_id = snapshot.id();

        // Build the row using the new id and the provided creator.
        // record_to_persisted_row produces a PersistedRow with
        // `record.id == snapshot.id() == source_id`; we patch the id
        // post-conversion to the freshly-minted DB id and update the
        // version FK targets to match.
        let mut row = record_to_persisted_row(snapshot, creator.to_string());
        row.record.id = new_db_id;
        for v in &mut row.versions {
            // Phase 4.D-fix-2 (2026-05-14): also re-mint per-version
            // ids so they don't collide with the source's persisted
            // versions. The in-memory `Record<Snapshot>` shares its
            // `Version.id` values with the source (cloned by the
            // camera-shutter snapshot, Phase 1.5 decision #17). Without
            // re-minting, the `CREATE version:<id>` in
            // [`write_record_atomic`] fails with AlreadyExists for
            // every version already present from the source's save.
            v.id = Uuid::now_v7();
            v.record_id = new_db_id;
        }

        write_record_atomic(self.surreal(), &row).await?;

        // Add the snapshot_of edge separately so the existence-check
        // semantics on save are preserved.
        self.surreal()
            .query(
                "RELATE (type::record('record', $new_id)) -> snapshot_of -> \
                 (type::record('record', $src_id)) SET at = $at",
            )
            .bind(("new_id", new_db_id.to_string()))
            .bind(("src_id", source_id.to_string()))
            .bind(("at", row.attribution.at))
            .await?
            .check()?;

        Ok(new_db_id)
    }

    async fn find(&self, id: Uuid) -> Result<Option<RecordWithAttribution>, DbError> {
        self.assemble(id).await
    }

    async fn list_by_creator(
        &self,
        creator: &str,
    ) -> Result<Vec<RecordWithAttribution>, DbError> {
        let (creator_table, creator_id) = parse_thing_literal(creator)?;

        // Fetch all record ids the user created. Use SurrealDB graph
        // traversal `->created_by->record`, then project the inner
        // record's identifier (which IS its UUID v7 string).
        // v3 SurrealQL stricter ORDER BY: the sort column must appear in the
        // SELECT projection. Phase 4.D-fix-2 (2026-05-14).
        let mut response = self
            .surreal()
            .query(
                "SELECT type::string(out) AS record_thing, at FROM created_by \
                 WHERE in = type::record($ct, $cid) ORDER BY at ASC",
            )
            .bind(("ct", creator_table.to_string()))
            .bind(("cid", creator_id.to_string()))
            .await?;

        #[derive(Deserialize, SurrealValue)]
        struct ListRow {
            record_thing: String,
            // `at` retained in projection for the v3 ORDER BY rule; the
            // value itself is irrelevant to caller.
            at: DateTime<Utc>,
        }

        let rows: Vec<ListRow> = response.take(0)?;
        let ids: Vec<Uuid> = rows
            .into_iter()
            .filter_map(|r| {
                let _ = r.at; // suppress unused-field warning
                // record_thing is `record:`<uuid>`` (v3 backtick-wrapped);
                // strip them then recover the UUID suffix.
                let cleaned = strip_thing_backticks(&r.record_thing);
                cleaned
                    .split_once(':')
                    .and_then(|(_, id)| Uuid::parse_str(id).ok())
            })
            .collect();

        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(rwa) = self.assemble(id).await? {
                out.push(rwa);
            }
        }
        Ok(out)
    }

    async fn add_version(&self, record_id: Uuid, body: Body) -> Result<Version, DbError> {
        let version_id = Uuid::now_v7();
        let now = Utc::now();
        let persisted_body = match &body {
            Body::Inline(bytes) => PersistedBody::Inline {
                bytes: bytes.clone(),
            },
            Body::BlobRef { key, size } => PersistedBody::BlobRef {
                key: key.clone(),
                size: *size,
            },
        };

        // Atomic: CREATE version + UPDATE record.updated_at.
        self.surreal()
            .query(
                "BEGIN TRANSACTION; \
                 CREATE type::record('version', $vid) CONTENT $vrow; \
                 UPDATE type::record('record', $rid) SET updated_at = $now; \
                 COMMIT TRANSACTION;",
            )
            .bind(("vid", version_id.to_string()))
            .bind((
                "vrow",
                VersionRowWrite {
                    record_id,
                    body: persisted_body,
                    created_at: now,
                },
            ))
            .bind(("rid", record_id.to_string()))
            .bind(("now", now))
            .await?
            .check()?;

        Ok(Version {
            id: VersionId(version_id),
            body,
        })
    }

    async fn transition_to_fixed(
        &self,
        id: Uuid,
        hash: Sha256Hash,
    ) -> Result<Record<Fixed>, DbError> {
        // Read current state to obtain the trailing version id +
        // body — we need it both to compute the rewritten BlobRef key
        // and to reconstruct the returned `Record<Fixed>`.
        let Some(current) = self.assemble(id).await? else {
            return Err(DbError::EmptyVersionChain(id));
        };
        let RecordWithAttribution {
            record: current_record,
            attribution: _,
        } = current;

        // Reject if already Fixed (idempotent semantics — caller
        // probably has a stale view; surface explicitly).
        if let AnyRecord::Fixed(_) = current_record {
            return Err(DbError::StateMismatch {
                actual: "fixed".to_string(),
                requested: "mutable | snapshot",
            });
        }

        // Extract id, kind, versions from whichever non-Fixed state we
        // got. Both Mutable and Snapshot fall through here.
        let (id_back, kind, versions): (Uuid, Kind, Vec<Version>) = match current_record {
            AnyRecord::Mutable(r) => (r.id(), r.kind().clone(), r.versions().to_vec()),
            AnyRecord::Snapshot(r) => (r.id(), r.kind().clone(), r.versions().to_vec()),
            AnyRecord::Fixed(_) => unreachable!(),
        };
        debug_assert_eq!(id_back, id);

        // Rewrite the trailing BlobRef.key (decision #15, migratory
        // bird → fossilization). For Inline bodies the key concept
        // does not apply; only BlobRef variants get the
        // `/fixed/<hash>` rewrite.
        let mut new_versions = versions.clone();
        if let Some(last) = new_versions.last_mut()
            && let Body::BlobRef { size, .. } = last.body.clone()
        {
            last.body = Body::BlobRef {
                key: format!("/fixed/{hash}"),
                size,
            };
        }
        let last_version_id = new_versions
            .last()
            .ok_or(DbError::EmptyVersionChain(id))?
            .id
            .0;
        let last_body = match &new_versions.last().unwrap().body {
            Body::Inline(b) => PersistedBody::Inline { bytes: b.clone() },
            Body::BlobRef { key, size } => PersistedBody::BlobRef {
                key: key.clone(),
                size: *size,
            },
        };

        // Atomic: state UPDATE + content_hash + last version body
        // rewrite + updated_at refresh.
        let now = Utc::now();
        let hash_hex = hash.to_string();
        self.surreal()
            .query(
                "BEGIN TRANSACTION; \
                 UPDATE type::record('record', $rid) SET state = 'fixed', \
                   content_hash = $hash, updated_at = $now; \
                 UPDATE type::record('version', $vid) SET body = $body; \
                 COMMIT TRANSACTION;",
            )
            .bind(("rid", id.to_string()))
            .bind(("hash", hash_hex))
            .bind(("now", now))
            .bind(("vid", last_version_id.to_string()))
            .bind(("body", last_body))
            .await?
            .check()?;

        Ok(Record::<Fixed>::from_parts(id, kind, hash, new_versions))
    }

    async fn delete(&self, id: Uuid) -> Result<bool, DbError> {
        // Existence check up-front so we can return an accurate
        // boolean even though the cascade is idempotent.
        let id_str = id.to_string();
        let existing: Option<RecordRowDb> = self
            .surreal()
            .select(("record", id_str.as_str()))
            .await?;
        if existing.is_none() {
            return Ok(false);
        }

        // Atomic cascade: edges (created_by + snapshot_of) →
        // versions → record.
        self.surreal()
            .query(
                "BEGIN TRANSACTION; \
                 DELETE FROM created_by WHERE out = type::record('record', $rid); \
                 DELETE FROM snapshot_of WHERE in  = type::record('record', $rid); \
                 DELETE FROM snapshot_of WHERE out = type::record('record', $rid); \
                 DELETE FROM version WHERE record_id = $rid_uuid; \
                 DELETE type::record('record', $rid); \
                 COMMIT TRANSACTION;",
            )
            .bind(("rid", id.to_string()))
            .bind(("rid_uuid", id))
            .await?
            .check()?;

        Ok(true)
    }
}

// =============================================================================
// Free helper: write a record + versions + created_by edge atomically
// =============================================================================

/// Shared implementation for `save<S>` and `save_snapshot` — both
/// write a `record` row, all its `version` rows, and a `created_by`
/// edge in a single SurrealDB transaction.
async fn write_record_atomic(
    db: &Surreal<Any>,
    row: &PersistedRow,
) -> Result<(), DbError> {
    let (creator_table, creator_id) = parse_thing_literal(&row.attribution.creator)?;
    let id_str = row.record.id.to_string();

    // Build the multi-statement transaction. The version count is
    // dynamic so we materialise the SurrealQL string up front and
    // bind one `$v_<i>` per version.
    let mut sql = String::from("BEGIN TRANSACTION;\n");
    sql.push_str("CREATE type::record('record', $rid) CONTENT $rec_row;\n");
    for i in 0..row.versions.len() {
        sql.push_str(&format!(
            "CREATE type::record('version', $vid_{i}) CONTENT $vrow_{i};\n",
        ));
    }
    sql.push_str(
        "RELATE (type::record($ct, $cid)) -> created_by -> \
         (type::record('record', $rid)) SET at = $at;\n",
    );
    sql.push_str("COMMIT TRANSACTION;\n");

    // Bind common params.
    let mut q = db.query(sql);
    q = q
        .bind(("rid", id_str))
        .bind(("rec_row", RecordRowDb::from_persisted(&row.record)))
        .bind(("ct", creator_table.to_string()))
        .bind(("cid", creator_id.to_string()))
        .bind(("at", row.attribution.at));

    // Bind per-version params.
    for (i, v) in row.versions.iter().enumerate() {
        q = q
            .bind((format!("vid_{i}"), v.id.to_string()))
            .bind((format!("vrow_{i}"), VersionRowWrite::from_persisted(v)));
    }

    q.await?.check()?;
    Ok(())
}
