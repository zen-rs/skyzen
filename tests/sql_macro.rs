//! End-to-end tests for `sql!`.
//!
//! The macro's *parsing* is unit-tested inside `skyzen-macros`; what cannot be checked there is
//! that the expansion is a real query builder — that the placeholders it emits are the ones the
//! backend rewrites, that the captures reach the right columns, and that it works against a
//! transaction as well as a `Db`.

use skyzen::{sql, Column, FromRow};
use skyzen_services::{ColumnEnum, Db};
use skyzen_test::mock::InMemoryDb;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Column)]
enum State {
    Active,
    Suspended,
}

#[derive(Debug, PartialEq, Eq, FromRow)]
struct User {
    id: i64,
    login: String,
    state: State,
}

async fn seeded() -> InMemoryDb {
    let db = InMemoryDb::new().await.expect("in-memory sqlite connects");
    db.db()
        .query(
            "CREATE TABLE users (
                id INTEGER PRIMARY KEY,
                login TEXT NOT NULL,
                state TEXT NOT NULL,
                seats INTEGER NOT NULL
            )",
        )
        .execute()
        .await
        .expect("the schema is valid");

    for (id, login, state, seats) in [
        (1_i64, "ada", State::Active, 3_u32),
        (2, "grace", State::Suspended, 7),
        (3, "alan", State::Active, 7),
    ] {
        sql!(
            db.db(),
            "INSERT INTO users (id, login, state, seats) VALUES ({id}, {login}, {state}, {seats})"
        )
        .execute()
        .await
        .expect("the insert succeeds");
    }
    db
}

#[tokio::test]
async fn captures_bind_in_the_order_they_are_written() {
    let db = seeded().await;
    let user: User = sql!(
        db.db(),
        "SELECT id, login, state FROM users WHERE state = {State::Active} AND seats = {7_u32}"
    )
    .fetch_one()
    .await
    .expect("the row is found");

    assert_eq!(
        user,
        User {
            id: 3,
            login: "alan".to_owned(),
            state: State::Active,
        }
    );
}

/// The hazard the macro exists to remove: this query is the one above with a condition spliced
/// into the middle. Nothing had to be renumbered, and each value is still read beside its column.
#[tokio::test]
async fn a_condition_added_in_the_middle_keeps_every_value_with_its_column() {
    let db = seeded().await;
    let user: User = sql!(
        db.db(),
        "SELECT id, login, state FROM users
         WHERE state = {State::Active} AND login = {\"alan\"} AND seats = {7_u32}"
    )
    .fetch_one()
    .await
    .expect("the row is found");
    assert_eq!(user.id, 3);
}

#[tokio::test]
async fn a_capture_may_be_any_expression() {
    let db = seeded().await;
    let wanted = ["ada", "grace"];
    let login: String = sql!(db.db(), "SELECT login FROM users WHERE login = {wanted[1]}")
        .fetch_scalar()
        .await
        .expect("the row is found");
    assert_eq!(login, "grace");
}

#[tokio::test]
async fn a_capture_is_a_bound_value_and_never_substituted_text() {
    let db = seeded().await;
    // If a capture were interpolated, this would truncate the table. It is bound, so it is only
    // ever a string compared against a column, and it matches nothing.
    let injection = "ada'; DROP TABLE users; --";
    let found: Option<String> = sql!(db.db(), "SELECT login FROM users WHERE login = {injection}")
        .fetch_scalar_optional()
        .await
        .expect("the query runs");
    assert_eq!(found, None);

    let survivors: i64 = sql!(db.db(), "SELECT COUNT(*) FROM users")
        .fetch_scalar()
        .await
        .expect("the table is still there");
    assert_eq!(survivors, 3);
}

#[tokio::test]
async fn doubled_braces_reach_the_backend_as_one_brace() {
    let db = InMemoryDb::new().await.expect("in-memory sqlite connects");
    let document: String = sql!(db.db(), "SELECT '{{\"kind\":\"email\"}}'")
        .fetch_scalar()
        .await
        .expect("the literal survives");
    assert_eq!(document, r#"{"kind":"email"}"#);
}

#[tokio::test]
async fn it_works_against_a_transaction_too() {
    let db = seeded().await;
    let mut tx = db.db().begin().await.expect("sqlite has transactions");
    sql!(
        tx,
        "UPDATE users SET state = {State::Suspended} WHERE id = {1_i64}"
    )
    .execute()
    .await
    .expect("the update runs");
    tx.commit().await.expect("the transaction commits");

    let state: State = sql!(db.db(), "SELECT state FROM users WHERE id = {1_i64}")
        .fetch_scalar()
        .await
        .expect("the row is found");
    assert_eq!(state, State::Suspended);
}

/// The macro emits the dialect-neutral `?`, and the builder rewrites it per backend — so a capture
/// count that disagrees with the statement is caught by the machinery that was already there.
#[tokio::test]
async fn the_emitted_placeholder_is_the_one_the_builder_rewrites() {
    let db = seeded().await;
    let ids: Vec<i64> = sql!(
        db.db(),
        "SELECT id FROM users WHERE seats = {7_u32} ORDER BY id"
    )
    .fetch_scalars()
    .await
    .expect("the query runs");
    assert_eq!(ids, vec![2, 3]);
}

#[test]
fn the_column_tokens_are_what_the_macro_binds() {
    assert_eq!(State::TOKENS, ["active", "suspended"]);
}

/// `Db` reached through a reference, the way a handler holds it.
async fn count_active(db: &Db) -> i64 {
    sql!(
        db,
        "SELECT COUNT(*) FROM users WHERE state = {State::Active}"
    )
    .fetch_scalar()
    .await
    .expect("the count runs")
}

#[tokio::test]
async fn it_works_through_a_reference_the_way_a_handler_holds_one() {
    let db = seeded().await;
    assert_eq!(count_active(db.db()).await, 2);
}
