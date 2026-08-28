//! Typed decoding of the rows a backend hands back.
//!
//! Binding is typed through [`DbValue`](crate::sql::DbValue): a `Uuid`, a `DateTime<Utc>` or a
//! `BigDecimal` goes to the database as itself. This module is the other direction. Rows travel
//! between backends as JSON — see the module docs on [`crate::sql`] for why, and for what each
//! backend renders — and [`FromColumn`] is the one place that says which of those JSON shapes a
//! given Rust type accepts. A `Uuid` column is a string on `PostgreSQL` and sixteen bytes on
//! `SQLite`, and the caller writes `Uuid` either way.
//!
//! [`FromRow`] composes those column decodes into a struct, and `#[derive(FromRow)]` writes the
//! composition for you:
//!
//! ```ignore
//! #[derive(skyzen::FromRow)]
//! struct Order {
//!     id: Uuid,
//!     #[row(rename = "customer_id")]
//!     customer: CustomerId,
//!     placed_at: DateTime<Utc>,
//!     total: BigDecimal,
//!     #[row(json)]
//!     items: Vec<LineItem>,
//! }
//! ```
//!
//! A single-column query needs no struct at all — see
//! [`SqlQuery::fetch_scalar`](crate::sql::SqlQuery::fetch_scalar).

use core::fmt;
use std::str::FromStr;

use bigdecimal::BigDecimal;
use chrono::{DateTime, NaiveDateTime, Utc};
use serde::de::DeserializeOwned;
use serde_json::Value;
use uuid::Uuid;

/// A type stored in one SQL column, read back from the portable JSON form.
///
/// The mirror of the <code>From&lt;T&gt; for [DbValue](crate::sql::DbValue)</code> set on the bind
/// side. Each implementation names every JSON shape the backends can produce for it, so that a
/// column round-trips whichever backend it was written on — `Uuid` accepts both the string
/// `PostgreSQL` returns and the sixteen-byte array `SQLite` and `MySQL` return, and an integer
/// accepts both a JSON number and the string a `NUMERIC` column renders as.
///
/// [`Option<T>`] is implemented for every `T`, and is the only thing that accepts `NULL`: a
/// non-optional field whose column is null fails, rather than substituting a default.
pub trait FromColumn: Sized {
    /// Decode one column value.
    ///
    /// # Errors
    ///
    /// Returns a [`ColumnError`] when `value` is not a shape this type accepts, or when it is the
    /// right shape but out of range — an integer too wide for the field, a string that is not a
    /// UUID.
    fn from_column(value: &Value) -> Result<Self, ColumnError>;
}

/// A type built from a whole result row.
///
/// Implemented by `#[derive(FromRow)]`, which reads one column per field with [`FromColumn`].
/// Implementing it by hand is worth it when a row does not map field-for-field onto a struct —
/// when two columns become one value, say.
pub trait FromRow: Sized {
    /// Decode one row.
    ///
    /// # Errors
    ///
    /// Returns a [`RowError`] when a column the type needs is absent, or holds something the
    /// field's type cannot decode.
    fn from_row(row: Row) -> Result<Self, RowError>;
}

/// One row of a result set.
///
/// Owns the columns the backend produced, in the portable JSON form described on [`crate::sql`].
/// A row is reached through [`FromRow`]; the accessors here are what a hand-written implementation
/// uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row(serde_json::Map<String, Value>);

impl Row {
    /// Take a row from the JSON object a backend produced.
    ///
    /// # Errors
    ///
    /// Returns [`RowError::NotAnObject`] if `value` is anything but a JSON object. Every backend
    /// renders a row as an object, so this is a backend bug rather than a caller mistake.
    pub fn try_from_value(value: Value) -> Result<Self, RowError> {
        match value {
            Value::Object(columns) => Ok(Self(columns)),
            other => Err(RowError::NotAnObject {
                found: describe(&other),
            }),
        }
    }

    /// The row as the JSON object it was built from.
    #[must_use]
    pub fn into_value(self) -> Value {
        Value::Object(self.0)
    }

    /// The column names this row carries.
    pub fn columns(&self) -> impl ExactSizeIterator<Item = &str> {
        self.0.keys().map(String::as_str)
    }

    /// How many columns this row carries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the row carries no columns at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The raw JSON value of one column.
    ///
    /// # Errors
    ///
    /// Returns [`RowError::MissingColumn`] if the query did not select `column`. A column that was
    /// selected and is `NULL` is present, and comes back as [`Value::Null`].
    pub fn column(&self, column: &str) -> Result<&Value, RowError> {
        self.0.get(column).ok_or_else(|| RowError::MissingColumn {
            column: column.to_owned(),
            available: self.0.keys().cloned().collect::<Vec<_>>().join(", "),
        })
    }

    /// Decode one column into `T`.
    ///
    /// # Errors
    ///
    /// Returns [`RowError::MissingColumn`] if the query did not select `column`, or
    /// [`RowError::Column`] if the value is not one `T` accepts.
    pub fn get<T: FromColumn>(&self, column: &str) -> Result<T, RowError> {
        T::from_column(self.column(column)?).map_err(|source| RowError::Column {
            column: column.to_owned(),
            source,
        })
    }

    /// Decode one column holding a JSON document into `T` with `serde`.
    ///
    /// This is what `#[row(json)]` generates. A backend with a native `JSON` column type hands
    /// the document back as JSON and it is deserialized directly; the backends that keep JSON in
    /// a text column (SQL Server, and `SQLite` unless the column was written as JSON) hand back a
    /// string, which is parsed first.
    ///
    /// # Errors
    ///
    /// Returns [`RowError::MissingColumn`] if the query did not select `column`, or
    /// [`RowError::Column`] if the document is not valid JSON for `T`.
    pub fn get_json<T: DeserializeOwned>(&self, column: &str) -> Result<T, RowError> {
        let value = self.column(column)?;
        let decoded = match value {
            Value::String(document) => serde_json::from_str(document),
            other => T::deserialize(other),
        };
        decoded.map_err(|error| RowError::Column {
            column: column.to_owned(),
            source: ColumnError::invalid("a JSON document for the field's type", value, &error),
        })
    }

    /// Decode the only column of a single-column row.
    ///
    /// # Errors
    ///
    /// Returns [`RowError::NotScalar`] unless the row has exactly one column, or
    /// [`RowError::Column`] if that column is not one `T` accepts.
    pub fn scalar<T: FromColumn>(&self) -> Result<T, RowError> {
        let mut columns = self.0.iter();
        match (columns.next(), columns.next()) {
            (Some((column, value)), None) => {
                T::from_column(value).map_err(|source| RowError::Column {
                    column: column.clone(),
                    source,
                })
            }
            _ => Err(RowError::NotScalar {
                count: self.0.len(),
            }),
        }
    }
}

impl TryFrom<Value> for Row {
    type Error = RowError;

    fn try_from(value: Value) -> Result<Self, Self::Error> {
        Self::try_from_value(value)
    }
}

impl FromRow for Row {
    fn from_row(row: Self) -> Result<Self, RowError> {
        Ok(row)
    }
}

impl FromRow for Value {
    fn from_row(row: Row) -> Result<Self, RowError> {
        Ok(row.into_value())
    }
}

/// A row deserialized by `serde` instead of column by column.
///
/// The escape hatch for a type that already has a `Deserialize` implementation someone else wrote
/// — the whole row object is handed to `serde`, exactly as it was before [`FromRow`] existed. It
/// buys nothing over `#[derive(FromRow)]` for a type you own: every column arrives as whatever
/// JSON its backend produced, which is what [`FromColumn`] exists to absorb.
///
/// ```ignore
/// let report: JsonRow<VendorReport> = db.query("SELECT * FROM reports").fetch_one().await?;
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct JsonRow<T>(pub T);

impl<T> JsonRow<T> {
    /// Unwrap the deserialized row.
    #[must_use]
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T: DeserializeOwned> FromRow for JsonRow<T> {
    fn from_row(row: Row) -> Result<Self, RowError> {
        Ok(Self(serde_json::from_value(row.into_value())?))
    }
}

/// A one-column row, so that a scalar query needs no struct.
///
/// Not public: it is reached through
/// [`SqlQuery::fetch_scalar`](crate::sql::SqlQuery::fetch_scalar) and its neighbours, which is
/// what keeps the scalar methods one line each rather than a second copy of the fetch logic.
pub struct Scalar<T>(pub T);

impl<T: FromColumn> FromRow for Scalar<T> {
    fn from_row(row: Row) -> Result<Self, RowError> {
        row.scalar().map(Self)
    }
}

/// A type whose values are a closed set of tokens, stored in a text column.
///
/// Generated by `#[derive(skyzen::Column)]` on an enum whose variants all have no fields. The
/// tokens are the variant names in `snake_case` unless `#[column(rename_all = "…")]` or
/// `#[column(rename = "…")]` says otherwise.
///
/// [`TOKENS`](Self::TOKENS) is the whole point of the trait rather than of the derive alone: it is
/// what lets a `CHECK (state IN (…))` constraint be checked against the type instead of drifting
/// away from it.
pub trait ColumnEnum: Sized + 'static {
    /// Every token a value of this type can be stored as, in declaration order.
    const TOKENS: &'static [&'static str];

    /// The token this value is stored as.
    fn token(&self) -> &'static str;

    /// The value a stored token names, or `None` when the database holds a token this build does
    /// not know.
    fn from_token(token: &str) -> Option<Self>;
}

/// Why one column could not be decoded into the type that asked for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnError {
    /// What the requesting type accepts, phrased for a reader ("an integer that fits in `u32`").
    expected: &'static str,
    /// What the row actually held.
    found: String,
    /// Why a value of the right shape was still refused — a parse failure, a range check.
    detail: Option<String>,
}

impl ColumnError {
    /// The column held a shape this type does not accept at all.
    #[must_use]
    pub fn unexpected(expected: &'static str, found: &Value) -> Self {
        Self {
            expected,
            found: describe(found),
            detail: None,
        }
    }

    /// The column held the right shape, but a value this type cannot represent.
    #[must_use]
    pub fn invalid(expected: &'static str, found: &Value, detail: &dyn fmt::Display) -> Self {
        Self {
            expected,
            found: describe(found),
            detail: Some(detail.to_string()),
        }
    }
}

impl fmt::Display for ColumnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "expected {}, found {}", self.expected, self.found)?;
        self.detail
            .as_ref()
            .map_or(Ok(()), |detail| write!(f, " ({detail})"))
    }
}

impl core::error::Error for ColumnError {}

/// Why a result row could not be decoded into the type that asked for it.
///
/// Every variant is a mismatch between the query and the type it was fetched into, which makes it
/// a programming error rather than a request error — [`DbError::Decode`](crate::sql::DbError)
/// renders as a 500.
#[derive(Debug, thiserror::Error)]
pub enum RowError {
    /// A backend produced a row that is not a JSON object.
    #[error("a result row is not an object: {found}")]
    NotAnObject {
        /// What the backend produced instead.
        found: String,
    },

    /// The query did not select a column the type needs.
    #[error("a result row has no column `{column}`; the row has: {available}")]
    MissingColumn {
        /// The column the type asked for.
        column: String,
        /// The columns the query actually selected, comma-separated.
        available: String,
    },

    /// A column held something the field's type cannot decode.
    #[error("column `{column}`: {source}")]
    Column {
        /// The column that could not be decoded.
        column: String,
        /// Why.
        #[source]
        source: ColumnError,
    },

    /// A scalar fetch ran against a query that does not select exactly one column.
    #[error("a scalar query must select exactly one column; this row has {count}")]
    NotScalar {
        /// How many columns the row has.
        count: usize,
    },

    /// A [`JsonRow`] could not be deserialized.
    #[error("a result row could not be deserialized: {0}")]
    Deserialize(#[from] serde_json::Error),
}

/// Render a column value for an error message, short enough to read in a log line.
fn describe(value: &Value) -> String {
    /// How much of a string value an error message quotes before eliding the rest.
    const MAX_QUOTED: usize = 48;

    match value {
        Value::Null => "null".to_owned(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::String(value) if value.chars().count() > MAX_QUOTED => {
            let head: String = value.chars().take(MAX_QUOTED).collect();
            format!("the string {head:?}…")
        }
        Value::String(value) => format!("the string {value:?}"),
        Value::Array(items) => format!("an array of {} elements", items.len()),
        Value::Object(columns) => format!("an object with {} keys", columns.len()),
    }
}

impl<T: FromColumn> FromColumn for Option<T> {
    fn from_column(value: &Value) -> Result<Self, ColumnError> {
        match value {
            Value::Null => Ok(None),
            value => T::from_column(value).map(Some),
        }
    }
}

impl FromColumn for Value {
    fn from_column(value: &Value) -> Result<Self, ColumnError> {
        Ok(value.clone())
    }
}

impl FromColumn for bool {
    /// `SQLite` and D1 have no boolean type and hand back the integer they stored, so `0` and `1`
    /// decode alongside a real JSON boolean.
    fn from_column(value: &Value) -> Result<Self, ColumnError> {
        match value {
            Value::Bool(value) => Ok(*value),
            Value::Number(number) => match number.as_u64() {
                Some(0) => Ok(false),
                Some(1) => Ok(true),
                _ => Err(ColumnError::unexpected(BOOL, value)),
            },
            value => Err(ColumnError::unexpected(BOOL, value)),
        }
    }
}

/// What a boolean field accepts, named once because two arms report it.
const BOOL: &str = "a boolean, or the integer 0 or 1";

/// The widest integer the portable row form can carry, before it is narrowed to the field's type.
#[derive(Debug, Clone, Copy)]
enum WideInteger {
    /// A value that fits in `i64`, which is every value a SQL `BIGINT` can hold.
    Signed(i64),
    /// A value above `i64::MAX`, which is how a `BIGINT UNSIGNED` column arrives.
    Unsigned(u64),
}

/// Decode any of the JSON forms an integer column arrives in.
///
/// A `NUMERIC`/`DECIMAL` column — which is what `SUM()` over integers returns on `PostgreSQL`, and
/// what a `u64` above `i64::MAX` is bound as — renders as a string, so a string that parses as an
/// integer is as valid a form as a JSON number. A fractional number is refused rather than
/// truncated.
fn wide_integer(value: &Value, expected: &'static str) -> Result<WideInteger, ColumnError> {
    match value {
        Value::Number(number) => match (number.as_i64(), number.as_u64()) {
            (Some(signed), _) => Ok(WideInteger::Signed(signed)),
            (None, Some(unsigned)) => Ok(WideInteger::Unsigned(unsigned)),
            (None, None) => Err(ColumnError::unexpected(expected, value)),
        },
        Value::String(text) => text
            .parse::<i64>()
            .map(WideInteger::Signed)
            .or_else(|_| text.parse::<u64>().map(WideInteger::Unsigned))
            .map_err(|error| ColumnError::invalid(expected, value, &error)),
        value => Err(ColumnError::unexpected(expected, value)),
    }
}

/// Decode an integer column and narrow it to the field's type, refusing anything out of range.
fn integer_column<T>(value: &Value, expected: &'static str) -> Result<T, ColumnError>
where
    T: TryFrom<i64> + TryFrom<u64>,
{
    let out_of_range = || ColumnError::invalid(expected, value, &"out of range");
    match wide_integer(value, expected)? {
        WideInteger::Signed(value) => T::try_from(value).map_err(|_| out_of_range()),
        WideInteger::Unsigned(value) => T::try_from(value).map_err(|_| out_of_range()),
    }
}

/// Implement [`FromColumn`] for every integer width over the one narrowing decoder.
macro_rules! integer_columns {
    ($($ty:ty),* $(,)?) => {
        $(
            impl FromColumn for $ty {
                fn from_column(value: &Value) -> Result<Self, ColumnError> {
                    integer_column(
                        value,
                        concat!("an integer that fits in `", stringify!($ty), "`"),
                    )
                }
            }
        )*
    };
}

integer_columns!(i8, i16, i32, i64, u8, u16, u32, u64);

impl FromColumn for f64 {
    fn from_column(value: &Value) -> Result<Self, ColumnError> {
        const EXPECTED: &str = "a number";
        match value {
            Value::Number(number) => number
                .as_f64()
                .ok_or_else(|| ColumnError::unexpected(EXPECTED, value)),
            // `NUMERIC` and `DECIMAL` columns render as strings to stay exact.
            Value::String(text) => text
                .parse()
                .map_err(|error| ColumnError::invalid(EXPECTED, value, &error)),
            value => Err(ColumnError::unexpected(EXPECTED, value)),
        }
    }
}

impl FromColumn for f32 {
    /// Narrowing loses precision the same way `REAL` does, but a magnitude `f32` would render as
    /// infinity is refused instead of being quietly discarded.
    #[allow(clippy::cast_possible_truncation)]
    fn from_column(value: &Value) -> Result<Self, ColumnError> {
        let wide = f64::from_column(value)?;
        if wide.is_finite() && wide.abs() > f64::from(Self::MAX) {
            return Err(ColumnError::invalid(
                "a number that fits in `f32`",
                value,
                &"out of range",
            ));
        }
        Ok(wide as Self)
    }
}

impl FromColumn for String {
    fn from_column(value: &Value) -> Result<Self, ColumnError> {
        match value {
            Value::String(text) => Ok(text.clone()),
            value => Err(ColumnError::unexpected("a string", value)),
        }
    }
}

impl FromColumn for Vec<u8> {
    /// JSON has no byte string, so every backend renders a blob as an array of byte values.
    fn from_column(value: &Value) -> Result<Self, ColumnError> {
        const EXPECTED: &str = "an array of byte values";
        match value {
            Value::Array(items) => items
                .iter()
                .map(|item| {
                    item.as_u64()
                        .and_then(|byte| u8::try_from(byte).ok())
                        .ok_or_else(|| {
                            ColumnError::invalid(EXPECTED, value, &"an element is not a byte")
                        })
                })
                .collect(),
            value => Err(ColumnError::unexpected(EXPECTED, value)),
        }
    }
}

impl FromColumn for Uuid {
    /// `DbValue::Uuid` binds as a native `UUID` on `PostgreSQL` and as sixteen bytes everywhere
    /// else, so both forms come back — and a UUID kept in a `TEXT` column comes back as a string
    /// too.
    fn from_column(value: &Value) -> Result<Self, ColumnError> {
        const EXPECTED: &str = "a UUID string, or an array of 16 bytes";
        match value {
            Value::String(text) => {
                Self::parse_str(text).map_err(|error| ColumnError::invalid(EXPECTED, value, &error))
            }
            Value::Array(_) => {
                let bytes = Vec::<u8>::from_column(value)?;
                let bytes: [u8; 16] = bytes
                    .try_into()
                    .map_err(|_| ColumnError::invalid(EXPECTED, value, &"not 16 bytes"))?;
                Ok(Self::from_bytes(bytes))
            }
            value => Err(ColumnError::unexpected(EXPECTED, value)),
        }
    }
}

impl FromColumn for DateTime<Utc> {
    /// A `TIMESTAMPTZ` renders as RFC 3339. Every other timestamp type has no zone to render, and
    /// arrives as the driver's textual form — `2024-05-06 07:08:09`, which is read as UTC because
    /// that is what [`DbValue::Timestamp`](crate::sql::DbValue) wrote. An integer is read as a
    /// Unix timestamp in **seconds**, which is the form `SQLite` schemas that predate Skyzen use.
    fn from_column(value: &Value) -> Result<Self, ColumnError> {
        const EXPECTED: &str =
            "an RFC 3339 timestamp, a `YYYY-MM-DD HH:MM:SS` timestamp, or Unix seconds";
        /// The naive forms a driver renders a zoneless timestamp column as.
        const NAIVE_FORMATS: [&str; 4] = [
            "%Y-%m-%d %H:%M:%S%.f",
            "%Y-%m-%dT%H:%M:%S%.f",
            "%Y-%m-%d %H:%M",
            "%Y-%m-%dT%H:%M",
        ];

        match value {
            Value::String(text) => {
                if let Ok(parsed) = Self::from_str(text) {
                    return Ok(parsed);
                }
                NAIVE_FORMATS
                    .iter()
                    .find_map(|format| NaiveDateTime::parse_from_str(text, format).ok())
                    .map(|naive| naive.and_utc())
                    .ok_or_else(|| ColumnError::unexpected(EXPECTED, value))
            }
            Value::Number(_) => {
                let seconds = integer_column::<i64>(value, EXPECTED)?;
                Self::from_timestamp(seconds, 0)
                    .ok_or_else(|| ColumnError::invalid(EXPECTED, value, &"out of range"))
            }
            value => Err(ColumnError::unexpected(EXPECTED, value)),
        }
    }
}

impl FromColumn for BigDecimal {
    /// `NUMERIC` and `DECIMAL` render as strings so the exact value survives; a plain number
    /// column read into a decimal is taken through its own rendering for the same reason.
    fn from_column(value: &Value) -> Result<Self, ColumnError> {
        const EXPECTED: &str = "an exact decimal";
        match value {
            Value::String(text) => {
                Self::from_str(text).map_err(|error| ColumnError::invalid(EXPECTED, value, &error))
            }
            Value::Number(number) => Self::from_str(&number.to_string())
                .map_err(|error| ColumnError::invalid(EXPECTED, value, &error)),
            value => Err(ColumnError::unexpected(EXPECTED, value)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ColumnEnum, FromColumn, FromRow, JsonRow, Row, RowError};
    use bigdecimal::BigDecimal;
    use chrono::{DateTime, Utc};
    use serde_json::json;
    use std::str::FromStr;
    use uuid::Uuid;

    fn row(value: serde_json::Value) -> Row {
        Row::try_from_value(value).expect("the fixture is an object")
    }

    #[test]
    fn integers_decode_from_every_form_a_backend_produces() {
        assert_eq!(i64::from_column(&json!(7)).expect("a number"), 7);
        // `NUMERIC` columns — which is what `SUM()` returns on PostgreSQL — render as strings.
        assert_eq!(i64::from_column(&json!("7")).expect("a string"), 7);
        // MySQL's `BIGINT UNSIGNED` exceeds `i64`, and `serde_json` keeps it exact.
        assert_eq!(
            u64::from_column(&json!(u64::MAX)).expect("an unsigned number"),
            u64::MAX
        );
        assert_eq!(
            u64::from_column(&json!(u64::MAX.to_string())).expect("a decimal string"),
            u64::MAX
        );
    }

    #[test]
    fn integers_refuse_what_they_cannot_represent() {
        // A negative value in a `u32` field is the read half of the `u64` clamp this replaces:
        // it fails loudly rather than wrapping.
        let error = u32::from_column(&json!(-1)).expect_err("negative is not a u32");
        assert!(error.to_string().contains("out of range"), "{error}");

        let error = u8::from_column(&json!(256)).expect_err("256 is not a byte");
        assert!(error.to_string().contains("out of range"), "{error}");

        // Truncating a fraction would be a silently different number.
        let error = i64::from_column(&json!(2.5)).expect_err("2.5 is not an integer");
        assert!(error.to_string().contains("expected an integer"), "{error}");
    }

    #[test]
    fn uuids_decode_from_both_the_string_and_the_byte_forms() {
        let uuid = Uuid::from_str("550e8400-e29b-41d4-a716-446655440000").expect("a valid UUID");
        assert_eq!(
            Uuid::from_column(&json!(uuid.to_string())).expect("the PostgreSQL form"),
            uuid
        );
        assert_eq!(
            Uuid::from_column(&json!(uuid.as_bytes().to_vec())).expect("the blob form"),
            uuid
        );
        let error = Uuid::from_column(&json!([1, 2, 3])).expect_err("three bytes is not a UUID");
        assert!(error.to_string().contains("not 16 bytes"), "{error}");
    }

    #[test]
    fn timestamps_decode_from_both_the_zoned_and_the_naive_renderings() {
        let expected = DateTime::<Utc>::from_str("2024-05-06T07:08:09Z").expect("valid RFC 3339");
        for form in [
            json!("2024-05-06T07:08:09Z"),
            json!("2024-05-06 07:08:09"),
            json!("2024-05-06T07:08:09"),
            json!(expected.timestamp()),
        ] {
            assert_eq!(
                DateTime::<Utc>::from_column(&form).expect("a timestamp"),
                expected,
                "{form}"
            );
        }
    }

    #[test]
    fn decimals_stay_exact_through_the_string_form() {
        assert_eq!(
            BigDecimal::from_column(&json!("19.99")).expect("a decimal"),
            BigDecimal::from_str("19.99").expect("a valid decimal")
        );
    }

    #[test]
    fn booleans_accept_the_integer_sqlite_stores() {
        assert!(bool::from_column(&json!(true)).expect("a boolean"));
        assert!(bool::from_column(&json!(1)).expect("SQLite's true"));
        assert!(!bool::from_column(&json!(0)).expect("SQLite's false"));
        bool::from_column(&json!(2)).expect_err("2 is not a boolean");
    }

    #[test]
    fn null_reaches_option_and_nothing_else() {
        assert_eq!(
            Option::<i64>::from_column(&json!(null)).expect("null"),
            None
        );
        i64::from_column(&json!(null)).expect_err("a non-optional field refuses null");
    }

    #[test]
    fn a_missing_column_names_the_ones_that_are_there() {
        let error = row(json!({ "id": 1 }))
            .get::<i64>("name")
            .expect_err("the column was not selected");
        assert!(
            matches!(&error, RowError::MissingColumn { column, available }
                if column == "name" && available == "id"),
            "{error}"
        );
    }

    #[test]
    fn a_failed_column_names_the_column() {
        let error = row(json!({ "id": "not a number" }))
            .get::<i64>("id")
            .expect_err("a string that is not an integer");
        assert!(
            matches!(&error, RowError::Column { column, .. } if column == "id"),
            "{error}"
        );
    }

    #[test]
    fn json_columns_decode_whether_the_backend_kept_them_as_json_or_as_text() {
        #[derive(Debug, PartialEq, Eq, serde::Deserialize)]
        struct Payload {
            kind: String,
        }

        let native = row(json!({ "payload": { "kind": "email" } }));
        let as_text = row(json!({ "payload": r#"{"kind":"email"}"# }));
        let expected = Payload {
            kind: "email".to_owned(),
        };
        assert_eq!(
            native.get_json::<Payload>("payload").expect("native JSON"),
            expected
        );
        assert_eq!(
            as_text
                .get_json::<Payload>("payload")
                .expect("JSON in text"),
            expected
        );
    }

    #[test]
    fn a_scalar_needs_exactly_one_column() {
        assert_eq!(row(json!({ "count": 2 })).scalar::<i64>().expect("one"), 2);
        let error = row(json!({ "count": 2, "total": 3 }))
            .scalar::<i64>()
            .expect_err("two columns are not a scalar");
        assert!(matches!(error, RowError::NotScalar { count: 2 }), "{error}");
    }

    #[test]
    fn a_row_that_is_not_an_object_is_refused_where_it_arrives() {
        let error = Row::try_from_value(json!([1, 2])).expect_err("an array is not a row");
        assert!(matches!(error, RowError::NotAnObject { .. }), "{error}");
    }

    #[test]
    fn json_row_hands_the_whole_row_to_serde() {
        #[derive(Debug, PartialEq, Eq, serde::Deserialize)]
        struct Vendor {
            id: i64,
        }

        let decoded = JsonRow::<Vendor>::from_row(row(json!({ "id": 4 }))).expect("serde decodes");
        assert_eq!(decoded.into_inner(), Vendor { id: 4 });
    }

    /// The trait's contract, checked against a hand-written implementation so the derive is not
    /// the only thing that can satisfy it.
    #[test]
    fn a_column_enum_round_trips_through_its_tokens() {
        #[derive(Debug, PartialEq, Eq)]
        enum State {
            AwaitingPayment,
            Shipped,
        }

        impl ColumnEnum for State {
            const TOKENS: &'static [&'static str] = &["awaiting_payment", "shipped"];

            fn token(&self) -> &'static str {
                match self {
                    Self::AwaitingPayment => "awaiting_payment",
                    Self::Shipped => "shipped",
                }
            }

            fn from_token(token: &str) -> Option<Self> {
                match token {
                    "awaiting_payment" => Some(Self::AwaitingPayment),
                    "shipped" => Some(Self::Shipped),
                    _ => None,
                }
            }
        }

        for state in [State::AwaitingPayment, State::Shipped] {
            assert_eq!(State::from_token(state.token()), Some(state));
        }
        assert_eq!(State::from_token("cancelled"), None);
    }
}
