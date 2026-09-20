//! Persistence.
//!
//! Split deliberately by access pattern: raw events go to an immutable,
//! append-only, compressed log (`segment`); everything that wants querying or
//! rewriting -- session metadata, health, context -- stays in small
//! human-readable files beside it.
//!
//! SQLITE, AND WHY IT IS NOT HERE YET
//! ----------------------------------
//! Spec section 11 asks for SQLite. It earns its place in Phase 3, when there
//! are derived tables (segments, clicks, feature vectors) that genuinely need
//! indexes and joins. In Phase 1 there is nothing to query: one header and an
//! append-only context log per session. Adding a bundled C dependency now would
//! buy nothing and put a second failure mode in front of the one thing Phase 1
//! must prove -- that the recorder does not lose events. The module boundary
//! here is drawn so that Phase 3 can add `storage::db` without the capture or
//! segment paths changing at all.

pub mod segment;
pub mod session;
pub mod writer;
