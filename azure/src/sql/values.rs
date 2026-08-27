//! Moving values between [`DbValue`] / the JSON row convention and TDS.
//!
//! # Binding
//!
//! tiberius carries a bound parameter as a [`ColumnData`], and infers the T-SQL type it declares to
//! `sp_executesql` from the variant. Every [`DbValue`] maps to one:
//!
//! | [`DbValue`] | [`ColumnData`] | Declared as |
//! | --- | --- | --- |
//! | `Null` | `String(None)` | `nvarchar(4000)` |
//! | `Boolean` | `Bit` | `bit` |
//! | `Integer` | `I64` | `bigint` |
//! | `Real` | `F64` | `float(53)` |
//! | `Text` | `String` | `nvarchar(4000)` / `nvarchar(max)` |
//! | `Blob` | `Binary` | `varbinary(8000)` / `varbinary(max)` |
//! | `Timestamp` | `DateTimeOffset` | `datetimeoffset` |
//! | `Uuid` | `Guid` | `uniqueidentifier` |
//! | `Decimal` | `Numeric` | `numeric` |
//! | `Json` | `String` | `nvarchar(max)` |
//!
//! Three of those rows are choices rather than the obvious mapping:
//!
//! - **A null binds as a null `nvarchar`**, because TDS has no untyped null: the parameter has to
//!   declare *some* type. SQL Server converts a null of one type to a null of another wherever the
//!   column or comparison needs it, so this works against every column, and it is the same thing
//!   the sqlx backends do by binding `Option::<String>::None`.
//! - **`Timestamp` binds as `datetimeoffset`**, at `+00:00`, rather than as the `datetime2`
//!   tiberius maps a `DateTime<Utc>` to on its own. The zone is the one thing a
//!   [`DbValue::Timestamp`] is certain about, and stating it is what makes the parameter's type the
//!   same as the column type this backend recommends. Written into a `datetime2` column SQL Server
//!   converts it, keeping the UTC wall clock — which is what the value is.
//! - **`Json` binds as `nvarchar`**, because SQL Server has no JSON column type in the versions
//!   this targets: `JSON_VALUE`, `OPENJSON` and `ISJSON` all read `nvarchar`, so the document is
//!   bound as its serialized text and every one of them works on it unchanged.
//!
//! `Decimal` is the one variant that can be *refused*. T-SQL's `numeric` holds 38 digits of
//! precision and 37 of scale; a [`BigDecimal`] needing more cannot be sent without losing digits,
//! so it is an error rather than a silent rounding — see [`numeric_from_decimal`].
//!
//! # Rows
//!
//! Rows convert into the same JSON shape every other Skyzen backend produces — one object per row,
//! keyed by column name — with the same documented lossiness: blobs become arrays of byte values,
//! `numeric` and `decimal` become exact strings rather than being rounded through a double, `NaN`
//! and infinity become `null`, and dates, times and UUIDs become strings.
//!
//! Which textual form a timestamp takes is decided by the **column**, not by what was bound:
//!
//! - a `datetimeoffset` column carries a zone and is rendered as RFC 3339, so a
//!   `chrono::DateTime<Utc>` field round-trips — this is the type to declare a timestamp column as;
//! - `datetime2`, `datetime` and `smalldatetime` carry none and are rendered in `chrono`'s own
//!   textual form, which a `chrono::NaiveDateTime` field deserializes and a `DateTime<Utc>` field
//!   does not. That form is passed through as `chrono` writes it rather than being guessed at and
//!   re-rendered as UTC: a wrong guess would silently shift every timestamp in the column instead
//!   of failing. The sqlx `PostgreSQL` backend draws the same line between `TIMESTAMPTZ` and
//!   `TIMESTAMP`.

use core::fmt::Display;

use bigdecimal::{BigDecimal, ToPrimitive as _};
use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime};
use deadpool_tiberius::tiberius::{
    numeric::Numeric, Column, ColumnData, FromSql as _, IntoSql, Row,
};
use serde_json::{Map, Number, Value};
use skyzen_services::{DbError, DbValue};

/// T-SQL's largest `numeric` precision, in digits.
const MAX_PRECISION: usize = 38;

/// The largest scale [`Numeric`] accepts — it panics at 38 rather than returning an error, so this
/// bound is checked here before one is ever constructed.
const MAX_SCALE: i64 = 37;

/// One bound parameter, on its way to tiberius.
///
/// tiberius binds through [`IntoSql`], and implements it for Rust types rather than for
/// [`ColumnData`] itself — so mapping a [`DbValue`] onto an exact TDS type, which is what the table
/// above is, needs this newtype to hand the result back. It is the whole reason the mapping is
/// testable without a server: [`ColumnData`] is `PartialEq`, so a test can assert the exact type a
/// value will be sent as.
#[derive(Debug, Clone, PartialEq)]
pub struct Param(pub ColumnData<'static>);

impl<'a> IntoSql<'a> for Param {
    fn into_sql(self) -> ColumnData<'a> {
        self.0
    }
}

/// Map one [`DbValue`] onto the TDS type it is sent as.
///
/// # Errors
///
/// [`DbError::Backend`] when a [`DbValue::Decimal`] does not fit T-SQL's `numeric`, and
/// [`DbError::Serialization`] when a [`DbValue::Json`] cannot be serialized.
pub fn to_param(value: &DbValue) -> Result<Param, DbError> {
    Ok(Param(match value {
        DbValue::Null => ColumnData::String(None),
        DbValue::Boolean(value) => ColumnData::Bit(Some(*value)),
        DbValue::Integer(value) => ColumnData::I64(Some(*value)),
        DbValue::Real(value) => ColumnData::F64(Some(*value)),
        DbValue::Text(value) => ColumnData::String(Some(value.clone().into())),
        DbValue::Blob(value) => ColumnData::Binary(Some(value.clone().into())),
        // Through `FixedOffset` rather than binding the `DateTime<Utc>` directly: tiberius maps a
        // `DateTime<Utc>` to `datetime2`, which carries no zone, and the zone is the one thing a
        // `DbValue::Timestamp` is certain about. The TDS date arithmetic is still tiberius's — it
        // is 30 lines this crate has no business repeating.
        DbValue::Timestamp(value) => DateTime::<FixedOffset>::from(*value).into_sql(),
        DbValue::Uuid(value) => ColumnData::Guid(Some(*value)),
        DbValue::Decimal(value) => ColumnData::Numeric(Some(numeric_from_decimal(value)?)),
        DbValue::Json(value) => ColumnData::String(Some(serde_json::to_string(value)?.into())),
    }))
}

/// Convert a [`BigDecimal`] into the exact `numeric` T-SQL will store.
///
/// [`Numeric`] is an `i128` and a scale, which is exactly what T-SQL's `numeric` is — so the
/// conversion is lossless whenever the value fits, and refused when it does not. It is refused
/// rather than rounded because a payment amount that quietly loses its last digits is the kind of
/// bug that surfaces in an audit rather than in a test.
///
/// Two conversions happen on the way:
///
/// - a **negative exponent** — the form `1E+5` parses to — is an integer written compactly, so it
///   is rescaled to scale zero, which is exact;
/// - the mantissa is narrowed to `i128`, which is where a value beyond 38 digits fails.
///
/// # Errors
///
/// [`DbError::Backend`] naming the value and the limit it exceeded.
fn numeric_from_decimal(value: &BigDecimal) -> Result<Numeric, DbError> {
    let (mantissa, scale) = match value.as_bigint_and_exponent() {
        (_, exponent) if exponent < 0 => (value.with_scale(0).as_bigint_and_exponent().0, 0),
        (mantissa, exponent) => (mantissa, exponent),
    };

    if scale > MAX_SCALE {
        return Err(DbError::backend(format!(
            "the decimal `{value}` has a scale of {scale}, and T-SQL's `numeric` holds at most \
             {MAX_SCALE}; round it before binding rather than letting the database do it"
        )));
    }
    let scale = u8::try_from(scale).map_err(|_| {
        DbError::backend(format!(
            "the decimal `{value}` has a scale of {scale}, which is not a scale T-SQL can express"
        ))
    })?;

    let mantissa = mantissa.to_i128().ok_or_else(|| out_of_precision(value))?;
    if mantissa.unsigned_abs().to_string().len() > MAX_PRECISION {
        return Err(out_of_precision(value));
    }

    Ok(Numeric::new_with_scale(mantissa, scale))
}

/// The error a decimal too wide for T-SQL's `numeric` reports.
fn out_of_precision(value: &BigDecimal) -> DbError {
    DbError::backend(format!(
        "the decimal `{value}` needs more than {MAX_PRECISION} digits of precision, which is the \
         most T-SQL's `numeric` holds"
    ))
}

/// Convert one row into a JSON object keyed by column name.
///
/// # Errors
///
/// Whatever [`cells_to_json`] reports.
pub fn row_to_json(row: &Row) -> Result<Value, DbError> {
    cells_to_json(row.cells())
}

/// Convert a row's column/value pairs into a JSON object.
///
/// Split out from [`row_to_json`] because a [`Row`] can only be built by the driver's own decoder,
/// while a [`Column`] and a [`ColumnData`] can both be built by hand — so this is the seam at which
/// the whole conversion is testable without a server.
///
/// # Errors
///
/// [`DbError::Backend`] when a column has no name, since an unnamed column cannot key anything, and
/// whatever [`column_data_to_json`] reports for a value it cannot decode.
pub fn cells_to_json<'a>(
    cells: impl Iterator<Item = (&'a Column, &'a ColumnData<'static>)>,
) -> Result<Value, DbError> {
    let mut object = Map::new();
    for (position, (column, data)) in cells.enumerate() {
        if column.name().is_empty() {
            return Err(DbError::backend(format!(
                "the column at position {position} came back without a name, so its value cannot \
                 be keyed; give the expression an `AS` alias"
            )));
        }
        object.insert(column.name().to_owned(), column_data_to_json(data)?);
    }
    Ok(Value::Object(object))
}

/// Convert one column value into JSON, following the row conventions every backend shares.
///
/// # Errors
///
/// [`DbError::Backend`] when a date, time or timestamp does not decode — which would mean the
/// driver produced a TDS value it cannot itself read back, so it is reported rather than rendered
/// as `null`.
pub fn column_data_to_json(data: &ColumnData<'static>) -> Result<Value, DbError> {
    Ok(match data {
        ColumnData::U8(value) => integer(value.map(i64::from)),
        ColumnData::I16(value) => integer(value.map(i64::from)),
        ColumnData::I32(value) => integer(value.map(i64::from)),
        ColumnData::I64(value) => integer(*value),
        ColumnData::F32(value) => double(value.map(f64::from)),
        ColumnData::F64(value) => double(*value),
        ColumnData::Bit(value) => value.map_or(Value::Null, Value::Bool),
        ColumnData::String(value) => value
            .as_ref()
            .map_or(Value::Null, |text| Value::String(text.to_string())),
        ColumnData::Guid(value) => {
            value.map_or(Value::Null, |uuid| Value::String(uuid.to_string()))
        }
        // JSON has no byte string, so a blob is an array of byte values — which is what a `Vec<u8>`
        // field deserializes from.
        ColumnData::Binary(value) => value
            .as_ref()
            .map_or(Value::Null, |bytes| bytes_to_json(bytes)),
        // Exact, and JSON numbers are not: the row contract renders a decimal as a string.
        ColumnData::Numeric(value) => value.map_or(Value::Null, |numeric| {
            Value::String(render_numeric(numeric))
        }),
        ColumnData::Xml(value) => value
            .as_ref()
            .map_or(Value::Null, |xml| Value::String(xml.to_string())),
        ColumnData::DateTime(_) | ColumnData::SmallDateTime(_) | ColumnData::DateTime2(_) => {
            rendered::<NaiveDateTime>(data, "a datetime")?
        }
        ColumnData::Date(_) => rendered::<NaiveDate>(data, "a date")?,
        ColumnData::Time(_) => rendered::<NaiveTime>(data, "a time")?,
        // The one type carrying a zone, so the one rendered as RFC 3339 — matching what the sqlx
        // `PostgreSQL` backend does with a `TIMESTAMPTZ`.
        ColumnData::DateTimeOffset(_) => DateTime::<FixedOffset>::from_sql(data)
            .map_err(|error| undecodable("a datetimeoffset", error))?
            .map_or(Value::Null, |value| Value::String(value.to_rfc3339())),
    })
}

/// Render a column that only `chrono` knows how to read out of its TDS form.
fn rendered<'a, T>(data: &'a ColumnData<'static>, described: &str) -> Result<Value, DbError>
where
    T: FromSqlDisplay<'a>,
{
    Ok(T::from_sql(data)
        .map_err(|error| undecodable(described, error))?
        .map_or(Value::Null, |value| Value::String(value.to_string())))
}

/// The bound [`rendered`] needs, named so the `where` clause stays readable.
trait FromSqlDisplay<'a>: deadpool_tiberius::tiberius::FromSql<'a> + Display {}

impl<'a, T> FromSqlDisplay<'a> for T where T: deadpool_tiberius::tiberius::FromSql<'a> + Display {}

/// The error a value the driver cannot read back reports.
fn undecodable(described: &str, error: deadpool_tiberius::tiberius::error::Error) -> DbError {
    DbError::backend_with(
        format!("Azure SQL returned {described} the driver could not decode: {error}"),
        error,
    )
}

/// Render a `numeric` exactly, from the integer and the scale it is stored as.
///
/// Not [`Numeric`]'s own `Display`, which forwards to its derived `Debug` and prints the struct.
/// The sign lives on the whole value rather than on the integer part, which is why this works from
/// the raw value instead of `int_part`/`dec_part`: `-0.05` has an integer part of `0`, and `0` has
/// no sign to carry.
fn render_numeric(numeric: Numeric) -> String {
    let scale = usize::from(numeric.scale());
    let value = numeric.value();
    let mut digits = value.unsigned_abs().to_string();

    if scale == 0 {
        return if value < 0 {
            let mut rendered = String::with_capacity(digits.len() + 1);
            rendered.push('-');
            rendered.push_str(&digits);
            rendered
        } else {
            digits
        };
    }

    // At least one digit has to sit before the point, so `5` at scale 2 becomes `005` → `0.05`.
    if digits.len() <= scale {
        let padding = scale + 1 - digits.len();
        digits.insert_str(0, &"0".repeat(padding));
    }

    let point = digits.len() - scale;
    let mut rendered = String::with_capacity(digits.len() + 2);
    if value < 0 {
        rendered.push('-');
    }
    rendered.push_str(&digits[..point]);
    rendered.push('.');
    rendered.push_str(&digits[point..]);
    rendered
}

/// Render an integer column, or `null`.
fn integer(value: Option<i64>) -> Value {
    value.map_or(Value::Null, |value| Value::Number(Number::from(value)))
}

/// Render a floating-point column, or `null`.
///
/// JSON has neither `NaN` nor infinity, so both render as `null` — the same thing the sqlx and RDS
/// Data API converters do with them.
fn double(value: Option<f64>) -> Value {
    value
        .and_then(Number::from_f64)
        .map_or(Value::Null, Value::Number)
}

/// Render bytes as the array of byte values the row contract documents.
fn bytes_to_json(bytes: &[u8]) -> Value {
    Value::Array(
        bytes
            .iter()
            .map(|byte| Value::Number(Number::from(*byte)))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::{
        cells_to_json, column_data_to_json, render_numeric, to_param, BigDecimal, Column,
        ColumnData, Numeric,
    };
    use core::str::FromStr as _;
    use deadpool_tiberius::tiberius::{ColumnType, FromSql as _, IntoSql as _, Uuid};
    use serde_json::json;
    use skyzen_services::{DbError, DbValue};

    fn column(name: &str) -> Column {
        Column::new(name.to_owned(), ColumnType::NVarchar)
    }

    fn decimal(text: &str) -> BigDecimal {
        BigDecimal::from_str(text).expect("a valid decimal")
    }

    #[test]
    fn every_db_value_maps_to_the_tds_type_it_is_sent_as() {
        let timestamp = chrono::DateTime::<chrono::Utc>::from_str("2024-05-06T07:08:09.123456Z")
            .expect("rfc3339");
        let uuid = Uuid::from_u128(0x1234_5678_9abc_def0_1234_5678_9abc_def0);

        let cases: Vec<(DbValue, ColumnData<'static>)> = vec![
            (DbValue::Null, ColumnData::String(None)),
            (DbValue::Boolean(true), ColumnData::Bit(Some(true))),
            (DbValue::Integer(-7), ColumnData::I64(Some(-7))),
            (DbValue::Real(1.5), ColumnData::F64(Some(1.5))),
            (
                DbValue::Text("hello".to_owned()),
                ColumnData::String(Some("hello".into())),
            ),
            (
                DbValue::Blob(vec![1, 2, 3]),
                ColumnData::Binary(Some(vec![1, 2, 3].into())),
            ),
            (DbValue::Uuid(uuid), ColumnData::Guid(Some(uuid))),
            (
                DbValue::Decimal(decimal("12.3400")),
                ColumnData::Numeric(Some(Numeric::new_with_scale(123_400, 4))),
            ),
            (
                DbValue::Json(json!({ "kind": "email" })),
                ColumnData::String(Some(r#"{"kind":"email"}"#.into())),
            ),
        ];

        for (value, expected) in cases {
            let param = to_param(&value).unwrap_or_else(|error| panic!("{value:?}: {error}"));
            assert_eq!(param.0, expected, "{value:?}");
        }

        // A timestamp states its zone rather than going out as the zoneless `datetime2` tiberius
        // would map a `DateTime<Utc>` to on its own...
        let param = to_param(&DbValue::Timestamp(timestamp)).expect("a timestamp binds");
        let ColumnData::DateTimeOffset(Some(sent)) = param.0 else {
            panic!(
                "a timestamp should bind as a datetimeoffset, got {:?}",
                param.0
            );
        };
        // ...and it survives the trip out and back unchanged.
        assert_eq!(
            chrono::DateTime::<chrono::Utc>::from_sql(&ColumnData::DateTimeOffset(Some(sent)))
                .expect("the driver reads back what it wrote")
                .expect("not null"),
            timestamp
        );
    }

    #[test]
    fn a_decimal_too_wide_for_t_sql_is_refused_rather_than_rounded() {
        // 39 digits: one more than `numeric` holds.
        let error = to_param(&DbValue::Decimal(decimal(&"9".repeat(39))))
            .expect_err("a value beyond 38 digits cannot be sent");
        assert!(error.to_string().contains("38 digits"), "{error}");

        let error = to_param(&DbValue::Decimal(decimal("0.1").with_scale(38)))
            .expect_err("a scale beyond 37 cannot be sent");
        assert!(error.to_string().contains("scale"), "{error}");
        assert!(matches!(error, DbError::Backend { .. }), "{error:?}");
    }

    #[test]
    fn a_decimal_written_as_a_power_of_ten_binds_as_the_integer_it_is() {
        // `1E+5` parses with a negative exponent, which T-SQL has no way to express; rescaling to
        // an integer is exact.
        let param = to_param(&DbValue::Decimal(decimal("1E+5"))).expect("an integer binds");
        assert_eq!(
            param.0,
            ColumnData::Numeric(Some(Numeric::new_with_scale(100_000, 0)))
        );
    }

    #[test]
    fn a_decimal_at_the_precision_limit_still_binds() {
        let param = to_param(&DbValue::Decimal(decimal(&"9".repeat(38))))
            .expect("exactly 38 digits is within `numeric`");
        assert!(matches!(param.0, ColumnData::Numeric(Some(_))), "{param:?}");
    }

    #[test]
    fn a_numeric_renders_exactly_including_its_sign_and_leading_zero() {
        // `-0.05` is the case an `int_part`/`dec_part` rendering gets wrong: the integer part is
        // `0`, which carries no sign.
        assert_eq!(render_numeric(Numeric::new_with_scale(-5, 2)), "-0.05");
        assert_eq!(render_numeric(Numeric::new_with_scale(5, 1)), "0.5");
        assert_eq!(render_numeric(Numeric::new_with_scale(0, 2)), "0.00");
        // Trailing zeros are significant in a `numeric`, so they survive.
        assert_eq!(
            render_numeric(Numeric::new_with_scale(123_400, 4)),
            "12.3400"
        );
        assert_eq!(render_numeric(Numeric::new_with_scale(-7, 0)), "-7");
        let widest = 10_i128.pow(37) - 1;
        assert_eq!(
            render_numeric(Numeric::new_with_scale(widest, 0)),
            "9".repeat(37)
        );
    }

    #[test]
    fn a_row_is_keyed_by_column_and_typed_per_value() {
        let columns = [
            column("id"),
            column("name"),
            column("score"),
            column("active"),
            column("avatar"),
            column("amount"),
            column("missing"),
        ];
        let values = [
            ColumnData::I32(Some(7)),
            ColumnData::String(Some("ada".into())),
            ColumnData::F64(Some(1.5)),
            ColumnData::Bit(Some(true)),
            ColumnData::Binary(Some(vec![0, 255].into())),
            ColumnData::Numeric(Some(Numeric::new_with_scale(1999, 2))),
            ColumnData::String(None),
        ];

        assert_eq!(
            cells_to_json(columns.iter().zip(values.iter())).expect("the row converts"),
            json!({
                "id": 7,
                "name": "ada",
                "score": 1.5,
                "active": true,
                "avatar": [0, 255],
                "amount": "19.99",
                "missing": null,
            })
        );
    }

    #[test]
    fn an_unnamed_column_is_an_error_that_says_what_to_do() {
        let columns = [column("")];
        let values = [ColumnData::I32(Some(1))];
        let error = cells_to_json(columns.iter().zip(values.iter()))
            .expect_err("an unnamed column cannot key anything");
        assert!(error.to_string().contains("AS"), "{error}");
    }

    #[test]
    fn a_uuid_comes_back_as_the_hyphenated_string_a_typed_field_deserializes_from() {
        let uuid = Uuid::from_u128(0x550e_8400_e29b_41d4_a716_4466_5544_0000);
        assert_eq!(
            column_data_to_json(&ColumnData::Guid(Some(uuid))).expect("a guid converts"),
            json!("550e8400-e29b-41d4-a716-446655440000")
        );
    }

    #[test]
    fn a_zoned_timestamp_comes_back_as_rfc_3339_and_a_zoneless_one_does_not() {
        let timestamp = chrono::DateTime::<chrono::Utc>::from_str("2024-05-06T07:08:09.123456Z")
            .expect("rfc3339");

        // What a `datetimeoffset` column returns: the form `DateTime<Utc>` deserializes from.
        let zoned = to_param(&DbValue::Timestamp(timestamp)).expect("binds").0;
        let rendered = column_data_to_json(&zoned).expect("converts");
        let rendered = rendered.as_str().expect("a string");
        assert_eq!(
            chrono::DateTime::parse_from_rfc3339(rendered).expect("rfc3339"),
            timestamp
        );

        // What a `datetime2` column returns: `chrono`'s naive form, which carries no zone — so a
        // `DateTime<Utc>` field will not deserialize it and a `NaiveDateTime` field will.
        let zoneless = timestamp.naive_utc().into_sql();
        let rendered = column_data_to_json(&zoneless).expect("converts");
        let rendered = rendered.as_str().expect("a string");
        assert!(
            chrono::DateTime::parse_from_rfc3339(rendered).is_err(),
            "{rendered}"
        );
        assert_eq!(
            chrono::NaiveDateTime::parse_from_str(rendered, "%Y-%m-%d %H:%M:%S%.f")
                .expect("chrono's own zoneless form"),
            timestamp.naive_utc()
        );
    }

    #[test]
    fn a_date_and_a_time_come_back_as_their_own_textual_forms() {
        let date = chrono::NaiveDate::from_ymd_opt(2024, 5, 6).expect("a valid date");
        let time = chrono::NaiveTime::from_hms_opt(7, 8, 9).expect("a valid time");
        assert_eq!(
            column_data_to_json(&date.into_sql()).expect("converts"),
            json!("2024-05-06")
        );
        assert_eq!(
            column_data_to_json(&time.into_sql()).expect("converts"),
            json!("07:08:09")
        );
    }

    #[test]
    fn a_double_that_json_cannot_hold_becomes_null() {
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(
                column_data_to_json(&ColumnData::F64(Some(value))).expect("converts"),
                json!(null),
                "{value}"
            );
        }
    }

    #[test]
    fn every_null_converts_to_a_json_null() {
        let nulls = [
            ColumnData::U8(None),
            ColumnData::I16(None),
            ColumnData::I32(None),
            ColumnData::I64(None),
            ColumnData::F32(None),
            ColumnData::F64(None),
            ColumnData::Bit(None),
            ColumnData::String(None),
            ColumnData::Guid(None),
            ColumnData::Binary(None),
            ColumnData::Numeric(None),
            ColumnData::Xml(None),
            ColumnData::DateTime(None),
            ColumnData::SmallDateTime(None),
            ColumnData::Time(None),
            ColumnData::Date(None),
            ColumnData::DateTime2(None),
            ColumnData::DateTimeOffset(None),
        ];
        for data in &nulls {
            assert!(
                column_data_to_json(data).expect("converts").is_null(),
                "{data:?}"
            );
        }
    }

    #[test]
    fn the_narrow_integer_types_widen_rather_than_stringifying() {
        assert_eq!(
            column_data_to_json(&ColumnData::U8(Some(255))).expect("converts"),
            json!(255)
        );
        assert_eq!(
            column_data_to_json(&ColumnData::I16(Some(-1))).expect("converts"),
            json!(-1)
        );
        assert_eq!(
            column_data_to_json(&ColumnData::I64(Some(i64::MAX))).expect("converts"),
            json!(i64::MAX)
        );
    }
}
