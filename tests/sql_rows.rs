//! End-to-end tests for `#[derive(FromRow)]` and `#[derive(Column)]`.
//!
//! Both derives expand to paths through `skyzen`, so they can only be exercised from a crate that
//! depends on it — which is why these live here rather than in `skyzen-services` beside the traits
//! they implement. Everything runs against a real `SQLite` database, so a decode that only works
//! against hand-written JSON would fail here.

use bigdecimal::BigDecimal;
use chrono::{DateTime, Utc};
use skyzen::{Column, FromRow};
use skyzen_services::{ColumnEnum, Db, DbValue};
use skyzen_test::mock::InMemoryDb;
use std::str::FromStr;
use uuid::Uuid;

/// A domain id, so the handler never passes a bare `Uuid` to the wrong parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Column)]
struct CustomerId(Uuid);

/// A state machine stored as its token, the way a `CHECK (state IN (…))` column wants it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Column)]
enum OrderState {
    AwaitingPayment,
    Shipped,
    #[column(rename = "cancelled")]
    Canceled,
}

#[derive(Debug, PartialEq, serde::Deserialize, serde::Serialize)]
struct LineItem {
    sku: String,
    quantity: u32,
}

#[derive(Debug, PartialEq, FromRow)]
struct Order {
    id: Uuid,
    #[row(rename = "customer")]
    customer_id: CustomerId,
    state: OrderState,
    placed_at: DateTime<Utc>,
    total: BigDecimal,
    /// Microdollars: the motivating case for an unsigned column that a signed `BIGINT` holds.
    budget: u64,
    shipped: bool,
    note: Option<String>,
    #[row(json)]
    items: Vec<LineItem>,
}

async fn db() -> InMemoryDb {
    let db = InMemoryDb::new().await.expect("in-memory sqlite connects");
    db.db()
        .query(
            "CREATE TABLE orders (
                id BLOB NOT NULL,
                customer BLOB NOT NULL,
                state TEXT NOT NULL,
                placed_at TEXT NOT NULL,
                total TEXT NOT NULL,
                budget INTEGER,
                shipped INTEGER NOT NULL,
                note TEXT,
                items TEXT NOT NULL
            )",
        )
        .execute()
        .await
        .expect("the schema is valid");
    db
}

fn fixture() -> Order {
    Order {
        id: Uuid::from_str("550e8400-e29b-41d4-a716-446655440000").expect("a valid UUID"),
        customer_id: CustomerId(
            Uuid::from_str("67e55044-10b1-426f-9247-bb680e5fe0c8").expect("a valid UUID"),
        ),
        state: OrderState::AwaitingPayment,
        placed_at: DateTime::<Utc>::from_str("2024-05-06T07:08:09Z").expect("valid RFC 3339"),
        total: BigDecimal::from_str("19.99").expect("a valid decimal"),
        budget: 4_500_000,
        shipped: false,
        note: None,
        items: vec![LineItem {
            sku: "SKU-1".to_owned(),
            quantity: 2,
        }],
    }
}

async fn insert(db: &Db, order: &Order) {
    db.query(
        "INSERT INTO orders (id, customer, state, placed_at, total, budget, shipped, note, items)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(order.id)
    .bind(order.customer_id)
    .bind(order.state)
    .bind(order.placed_at)
    .bind(order.total.clone())
    .bind(order.budget)
    .bind(order.shipped)
    .bind(order.note.clone())
    .bind(serde_json::to_value(&order.items).expect("line items serialize"))
    .execute()
    .await
    .expect("the insert succeeds");
}

#[tokio::test]
async fn a_derived_row_round_trips_every_typed_column() {
    let db = db().await;
    let expected = fixture();
    insert(db.db(), &expected).await;

    let order: Order = db
        .db()
        .query("SELECT * FROM orders")
        .fetch_one()
        .await
        .expect("the row decodes");
    assert_eq!(order, expected);
}

#[tokio::test]
async fn a_renamed_field_reads_the_column_it_names() {
    let db = db().await;
    let expected = fixture();
    insert(db.db(), &expected).await;

    // `customer_id` is spelled `customer` in the schema, and the query selects it by that name.
    let customer: CustomerId = db
        .db()
        .query("SELECT customer FROM orders")
        .fetch_scalar()
        .await
        .expect("the newtype decodes from the column it wraps");
    assert_eq!(customer, expected.customer_id);
}

#[tokio::test]
async fn a_unit_enum_binds_and_reads_as_its_token() {
    let db = db().await;
    let mut expected = fixture();
    expected.state = OrderState::Canceled;
    insert(db.db(), &expected).await;

    // What is actually in the column is the token, which is what makes a `CHECK` constraint
    // against `OrderState::TOKENS` meaningful.
    let stored: String = db
        .db()
        .query("SELECT state FROM orders")
        .fetch_scalar()
        .await
        .expect("the column holds text");
    assert_eq!(stored, "cancelled");

    let state: OrderState = db
        .db()
        .query("SELECT state FROM orders WHERE state = ?")
        .bind(OrderState::Canceled)
        .fetch_scalar()
        .await
        .expect("the token decodes back");
    assert_eq!(state, OrderState::Canceled);
}

#[test]
fn the_tokens_are_the_snake_case_variant_names_unless_renamed() {
    assert_eq!(
        OrderState::TOKENS,
        ["awaiting_payment", "shipped", "cancelled"]
    );
    assert_eq!(OrderState::from_token("shipped"), Some(OrderState::Shipped));
    assert_eq!(OrderState::from_token("Shipped"), None);
    assert!(matches!(
        DbValue::from(OrderState::Shipped),
        DbValue::Text(token) if token == "shipped"
    ));
}

#[tokio::test]
async fn a_token_the_build_does_not_know_is_an_error_naming_the_ones_it_does() {
    let db = db().await;
    let mut expected = fixture();
    expected.state = OrderState::Shipped;
    insert(db.db(), &expected).await;
    db.db()
        .query("UPDATE orders SET state = ?")
        .bind("refunded")
        .execute()
        .await
        .expect("the update succeeds");

    let error = db
        .db()
        .query("SELECT * FROM orders")
        .fetch_one::<Order>()
        .await
        .expect_err("`refunded` is not an OrderState");
    let message = error.to_string();
    assert!(message.contains("state"), "{message}");
    assert!(message.contains("awaiting_payment"), "{message}");
}

#[tokio::test]
async fn a_null_in_a_non_optional_column_is_refused_rather_than_defaulted() {
    let db = db().await;
    insert(db.db(), &fixture()).await;
    db.db()
        .query("UPDATE orders SET budget = NULL")
        .execute()
        .await
        .expect("the update succeeds");

    let error = db
        .db()
        .query("SELECT * FROM orders")
        .fetch_one::<Order>()
        .await
        .expect_err("a null budget has no `u64` to decode into");
    assert!(error.to_string().contains("budget"), "{error}");
}

#[tokio::test]
async fn a_column_the_query_forgot_names_the_ones_it_selected() {
    let db = db().await;
    insert(db.db(), &fixture()).await;

    let error = db
        .db()
        .query("SELECT id, customer FROM orders")
        .fetch_one::<Order>()
        .await
        .expect_err("the projection is missing most of the row");
    let message = error.to_string();
    assert!(message.contains("no column `state`"), "{message}");
    assert!(message.contains("customer"), "{message}");
}

#[tokio::test]
async fn an_unsigned_value_too_wide_for_a_signed_column_is_stored_exactly() {
    let db = InMemoryDb::new().await.expect("in-memory sqlite connects");
    // A value above `i64::MAX` is bound as an exact decimal, so it needs a column that holds one.
    // On `SQLite` that means `TEXT`, because every other affinity would convert it to a float.
    db.db()
        .query("CREATE TABLE budgets (microdollars TEXT NOT NULL)")
        .execute()
        .await
        .expect("the schema is valid");
    db.db()
        .query("INSERT INTO budgets (microdollars) VALUES (?)")
        .bind(u64::MAX)
        .execute()
        .await
        .expect("the insert succeeds");

    // Exact, not clamped: the number that comes back is the number that went in.
    let stored: String = db
        .db()
        .query("SELECT microdollars FROM budgets")
        .fetch_scalar()
        .await
        .expect("the column holds the rendered value");
    assert_eq!(stored, u64::MAX.to_string());

    let budget: u64 = db
        .db()
        .query("SELECT microdollars FROM budgets")
        .fetch_scalar()
        .await
        .expect("the stored value decodes");
    assert_eq!(budget, u64::MAX);
}
