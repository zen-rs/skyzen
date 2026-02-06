//! JSON fixture loading utilities.
//!
//! Provides helpers for loading test data from JSON files and strings.

use serde::de::DeserializeOwned;

/// Load a value from a JSON string.
///
/// # Errors
///
/// Returns a deserialization error if the JSON is invalid or does not match the target type.
pub fn from_json_str<T: DeserializeOwned>(json: &str) -> Result<T, serde_json::Error> {
    serde_json::from_str(json)
}

/// Load a `Vec<T>` from a JSON array string.
///
/// # Errors
///
/// Returns a deserialization error if the JSON is invalid or not an array of the target type.
pub fn from_json_array<T: DeserializeOwned>(json: &str) -> Result<Vec<T>, serde_json::Error> {
    serde_json::from_str(json)
}
