//! Real in-memory Durable Object SQL storage for tests.

/// An isolated SQLite-backed Durable Object database.
///
/// Create it with [`InMemoryDurableDb::in_memory`], then wrap a clone in
/// [`skyzen_services::durable::DurableDb`] for injection into a test context.
pub type InMemoryDurableDb = skyzen_services::durable::SqliteDurableDb;
