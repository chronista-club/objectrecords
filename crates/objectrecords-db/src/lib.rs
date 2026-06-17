//! `objectrecords-db` — SurrealDB persistence layer for Object Records.
//!
//! This crate maps the dependency-free [`objectrecords_core`] type-state
//! model onto a SurrealDB schema, following the Phase 3 design decisions:
//!
//! - **Decision #25** (axis A) — share the creo-memories SurrealDB instance,
//!   isolated by namespace (`objectrecords` for production,
//!   `objectrecords_test` for integration tests).
//! - **Decision #26** (axis B) — two-table schema: `record` and `version`,
//!   linked by FK. Version inserts are O(1), preserving the
//!   "annual-rings" semantics of [`objectrecords_core::Record::versions`].
//! - **Decision #27** (axis C) — DTO + `TryFrom`. The
//!   [`PersistedRecord`] / [`PersistedVersion`] DTOs live here, not in core,
//!   so core stays serde-free (decision #14).
//! - **Decision #28** (Sub-Q1 β) — depend on `surrealdb` directly; do not
//!   pull in creo-memories' `DbClient`.
//! - **Decision #29** (Sub-Q2 γ) — schema lives as `DEFINE TABLE` const
//!   strings under [`schema`], applied idempotently with `OVERWRITE`.
//!
//! The Phase 3.0 surface is intentionally narrow: only the data layer
//! (Persisted DTOs + `TryFrom` round-trips). The repository / live SurrealDB
//! integration follows in Phase 3.1+.

#![warn(missing_docs)]

pub mod client;
pub mod convert;
pub mod error;
pub mod persisted;
pub mod repository;
pub mod schema;

pub use client::{ObjectRecordsDb, ObjectRecordsDbConfig};
pub use repository::{
    AnyRecord, OrUser, RecordRepository, RecordWithAttribution, SurrealRecordRepository,
};

pub use convert::{StateLiteral, record_to_persisted_row};
pub use error::DbError;
pub use persisted::{
    PersistedAttribution, PersistedBody, PersistedRecord, PersistedRow, PersistedState,
    PersistedVersion,
};
