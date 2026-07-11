//! Snapshot testing utilities.
//!
//! Powered by the [`insta`] crate for automatic snapshot management.

use crate::assertions::TestResponse;

/// Extension trait adding snapshot assertion methods to [`TestResponse`].
pub trait SnapshotExt {
    /// Assert the response body matches a named snapshot.
    ///
    /// On first run, the snapshot is created. On subsequent runs, the body is
    /// compared against the saved snapshot. Use `cargo insta review` to manage snapshots.
    ///
    /// # Panics
    ///
    /// Panics if the response body does not match the stored snapshot.
    fn assert_snapshot(&self, name: &str);
}

impl SnapshotExt for TestResponse {
    fn assert_snapshot(&self, name: &str) {
        let body_text = self.body_text();

        // Try to parse as JSON for pretty snapshot, fall back to raw text
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&body_text) {
            insta::assert_json_snapshot!(name, json);
        } else {
            insta::assert_snapshot!(name, body_text);
        }
    }
}
