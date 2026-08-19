//! Response assertion utilities for testing.

use http_kit::{header::HeaderMap, Response, StatusCode};
use serde::de::DeserializeOwned;

/// A captured HTTP response ready for assertions.
///
/// The response body is fully read into memory so it can be inspected
/// multiple times without consuming it.
#[derive(Debug)]
pub struct TestResponse {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl TestResponse {
    /// Consume the response and read the body into memory.
    pub(crate) async fn new(response: Response) -> Self {
        let (parts, body) = response.into_parts();
        let body_bytes = body
            .into_bytes()
            .await
            .expect("failed to read response body in test");

        Self {
            status: parts.status,
            headers: parts.headers,
            body: body_bytes.to_vec(),
        }
    }

    // ── Status assertions ──

    /// Assert the response has a specific status code.
    ///
    /// # Panics
    ///
    /// Panics if the status code does not match.
    pub fn assert_status(&self, expected: u16) {
        assert_eq!(
            self.status.as_u16(),
            expected,
            "expected status {expected}, got {} with body: {}",
            self.status.as_u16(),
            self.body_text(),
        );
    }

    /// Assert the response has a 2xx status code.
    ///
    /// # Panics
    ///
    /// Panics if the status code is not in the 200-299 range.
    pub fn assert_status_success(&self) {
        assert!(
            self.status.is_success(),
            "expected success status (2xx), got {} with body: {}",
            self.status.as_u16(),
            self.body_text(),
        );
    }

    /// Assert the response has a 4xx status code.
    ///
    /// # Panics
    ///
    /// Panics if the status code is not in the 400-499 range.
    pub fn assert_status_client_error(&self) {
        assert!(
            self.status.is_client_error(),
            "expected client error status (4xx), got {}",
            self.status.as_u16(),
        );
    }

    /// Assert the response has a 5xx status code.
    ///
    /// # Panics
    ///
    /// Panics if the status code is not in the 500-599 range.
    pub fn assert_status_server_error(&self) {
        assert!(
            self.status.is_server_error(),
            "expected server error status (5xx), got {}",
            self.status.as_u16(),
        );
    }

    // ── Header assertions ──

    /// Assert a response header has a specific value.
    ///
    /// # Panics
    ///
    /// Panics if the header is missing or has a different value.
    pub fn assert_header(&self, name: &str, expected: &str) {
        let actual = self
            .headers
            .get(name)
            .unwrap_or_else(|| panic!("expected header `{name}` to be present"))
            .to_str()
            .expect("header value is not valid UTF-8");
        assert_eq!(
            actual, expected,
            "expected header `{name}` to be `{expected}`, got `{actual}`",
        );
    }

    /// Assert a response header exists (regardless of value).
    ///
    /// # Panics
    ///
    /// Panics if the header is not present.
    pub fn assert_header_exists(&self, name: &str) {
        assert!(
            self.headers.contains_key(name),
            "expected header `{name}` to be present",
        );
    }

    // ── Body assertions ──

    /// Assert the body contains the given substring.
    ///
    /// # Panics
    ///
    /// Panics if the body does not contain the substring.
    pub fn assert_body_contains(&self, expected: &str) {
        let body = self.body_text();
        assert!(
            body.contains(expected),
            "expected body to contain `{expected}`, got: {body}",
        );
    }

    /// Deserialize the body as JSON and assert it succeeds.
    ///
    /// # Panics
    ///
    /// Panics if the body cannot be deserialized to the given type.
    #[must_use]
    pub fn assert_json<T: DeserializeOwned>(&self) -> T {
        serde_json::from_slice(&self.body).unwrap_or_else(|e| {
            panic!(
                "failed to deserialize response body as {}: {e}\nbody: {}",
                std::any::type_name::<T>(),
                self.body_text(),
            )
        })
    }

    /// Deserialize the body as JSON and assert a specific JSON path value.
    ///
    /// Uses a simple dot-separated path (e.g. `"data.id"`, `"users.0.name"`).
    ///
    /// # Panics
    ///
    /// Panics if the path does not exist or the value does not match.
    pub fn assert_json_path(&self, path: &str, expected: &serde_json::Value) {
        let json: serde_json::Value = self.assert_json();
        let actual = resolve_json_path(&json, path)
            .unwrap_or_else(|| panic!("JSON path `{path}` not found in response body"));
        assert_eq!(
            actual, expected,
            "at JSON path `{path}`: expected {expected}, got {actual}",
        );
    }

    // ── Accessors ──

    /// Get the response status code.
    #[must_use]
    pub const fn status(&self) -> StatusCode {
        self.status
    }

    /// Get the response headers.
    #[must_use]
    pub const fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    /// Get the response body as raw bytes.
    #[must_use]
    pub fn body_bytes(&self) -> &[u8] {
        &self.body
    }

    /// Get the response body as a UTF-8 string.
    #[must_use]
    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// Deserialize the response body from JSON.
    ///
    /// # Panics
    ///
    /// Panics if the body cannot be deserialized.
    #[must_use]
    pub fn json<T: DeserializeOwned>(&self) -> T {
        self.assert_json()
    }
}

/// Resolve a dot-separated path into a JSON value.
///
/// A numeric segment is only treated as an array index when the current value
/// is an array; for objects it is always looked up as a key, so object keys
/// like `"0"` remain addressable.
fn resolve_json_path<'a>(
    value: &'a serde_json::Value,
    path: &str,
) -> Option<&'a serde_json::Value> {
    let mut current = value;
    for segment in path.split('.') {
        current = match current {
            serde_json::Value::Array(items) => items.get(segment.parse::<usize>().ok()?)?,
            serde_json::Value::Object(map) => map.get(segment)?,
            _ => return None,
        };
    }
    Some(current)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::resolve_json_path;

    #[test]
    fn resolves_object_keys_and_array_indices() {
        let value = json!({"users": [{"name": "alice"}, {"name": "bob"}]});
        assert_eq!(
            resolve_json_path(&value, "users.1.name"),
            Some(&json!("bob"))
        );
    }

    #[test]
    fn numeric_segments_address_object_keys_when_value_is_an_object() {
        let value = json!({"counts": {"0": 10, "1": 20}});
        assert_eq!(resolve_json_path(&value, "counts.0"), Some(&json!(10)));
        assert_eq!(resolve_json_path(&value, "counts.1"), Some(&json!(20)));
    }

    #[test]
    fn missing_paths_resolve_to_none() {
        let value = json!({"a": [1, 2]});
        assert_eq!(resolve_json_path(&value, "a.5"), None);
        assert_eq!(resolve_json_path(&value, "a.not_an_index"), None);
        assert_eq!(resolve_json_path(&value, "b"), None);
        assert_eq!(resolve_json_path(&value, "a.0.deeper"), None);
    }
}
