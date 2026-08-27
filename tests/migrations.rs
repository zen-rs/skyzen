//! End-to-end tests for `embed_migrations!` and the migration runner.
//!
//! Everything here goes through the real macro and the real runner against a real `SQLite`
//! database. The macro's *rejections* are unit-tested inside `skyzen-macros`, because a compile
//! error cannot be observed from a test that has to compile.

use serde::Deserialize;
use skyzen::embed_migrations;
use skyzen_services::{Db, Migrations};
use skyzen_test::mock::InMemoryDb;

/// The application's migration set, embedded exactly as an application would embed it.
///
/// A `static` is the point of the expansion's shape: `#[skyzen::test(migrations = ...)]` names a
/// path, so the set has to live somewhere nameable.
static MIGRATIONS: Migrations = embed_migrations!("tests/fixtures/migrations");

#[derive(Debug, Deserialize, PartialEq, Eq)]
struct User {
    id: i64,
    email: String,
}

async fn users(db: &Db) -> Vec<User> {
    db.query("SELECT id, email FROM users ORDER BY id")
        .fetch_all()
        .await
        .expect("the users table should exist and be readable")
}

// Plain `#[test]`: what the macro embedded is settled at compile time, so these two assert on a
// `static` and never touch a database or a runtime.
#[test]
fn the_macro_reads_the_directory_in_version_order() {
    assert_eq!(MIGRATIONS.len(), 2);
    let versions: Vec<u64> = MIGRATIONS
        .iter()
        .map(skyzen_services::Migration::version)
        .collect();
    assert_eq!(versions, vec![1, 2]);
    assert_eq!(MIGRATIONS.as_slice()[0].name(), "create_users");
    assert_eq!(MIGRATIONS.as_slice()[1].name(), "seed_users");

    // The SQL reached the binary verbatim, comments and all.
    assert!(MIGRATIONS.as_slice()[0]
        .sql()
        .contains("CREATE TABLE users"));
    assert!(MIGRATIONS.as_slice()[1].sql().contains("semi;colon@"));
}

#[test]
fn the_embedded_checksum_matches_what_the_shared_reader_computes() {
    // The macro and `skyzen migrate` have to agree, or a deploy would report edited history for a
    // file nobody touched. Both go through `skyzen_manifest::migrations`, and this is the
    // assertion that keeps them wired to it.
    let directory = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/migrations");
    let files = skyzen_manifest::migrations::load(std::path::Path::new(directory))
        .expect("the fixture directory is valid");

    assert_eq!(files.len(), MIGRATIONS.len());
    for (file, embedded) in files.iter().zip(MIGRATIONS.iter()) {
        assert_eq!(file.version, embedded.version());
        assert_eq!(file.name, embedded.name());
        assert_eq!(file.checksum, embedded.checksum());
        assert_eq!(file.sql, embedded.sql());
    }
}

#[skyzen::test]
async fn with_migrations_builds_a_database_the_migrations_describe() {
    let db = InMemoryDb::with_migrations(&MIGRATIONS)
        .await
        .expect("the fixture migrations apply");

    assert_eq!(
        users(db.db()).await,
        vec![
            User {
                id: 1,
                email: "ada@example.invalid".to_owned()
            },
            User {
                id: 2,
                // The second statement of the second migration: proof the file was split on
                // statement boundaries and not on the semicolon inside this literal.
                email: "semi;colon@example.invalid".to_owned()
            },
        ]
    );
}

#[skyzen::test]
async fn migrating_twice_changes_nothing() {
    let db = InMemoryDb::with_migrations(&MIGRATIONS)
        .await
        .expect("first run");

    let report = db.migrate(&MIGRATIONS).await.expect("second run");
    assert!(report.is_empty(), "{report:?}");
    assert_eq!(report.skipped, MIGRATIONS.len());
    assert_eq!(users(db.db()).await.len(), 2);
}

#[skyzen::test]
async fn status_reports_the_set_as_fully_applied() {
    let db = InMemoryDb::with_migrations(&MIGRATIONS)
        .await
        .expect("migrations apply");

    let status = db
        .db()
        .migration_status(&MIGRATIONS)
        .await
        .expect("status should read");
    assert_eq!(status.applied.len(), 2);
    assert_eq!(status.pending.len(), 0);
    assert_eq!(
        status.applied[0].checksum,
        MIGRATIONS.as_slice()[0].checksum_hex()
    );
    assert!(!status.applied[0].applied_at.is_empty());
}

/// The attribute argument: the database a test receives is already migrated when the body starts.
#[skyzen::test(migrations = MIGRATIONS)]
async fn the_test_attribute_migrates_the_injected_database(db: Db) {
    assert_eq!(users(&db).await.len(), 2);

    db.query("INSERT INTO users (id, email) VALUES (?, ?)")
        .bind(3_i64)
        .bind("grace@example.invalid")
        .execute()
        .await
        .expect("the migrated schema accepts writes");
    assert_eq!(users(&db).await.len(), 3);
}

/// Each test gets its own database, so the migration set is applied per test rather than shared.
#[skyzen::test(migrations = MIGRATIONS)]
async fn each_test_gets_its_own_migrated_database(db: Db) {
    assert_eq!(users(&db).await.len(), 2);
}
