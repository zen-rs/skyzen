//! What a provider hands a deployed function, and how it merges with what is already there.
//!
//! Lambda's function environment and a Function App's application settings are the same shape —
//! one flat map of names to values, replaced wholesale by the call that writes it — so the read,
//! merge and name-listing are here rather than twice. What differs is only the transport, which is
//! each provider's own module.
//!
//! The values stay in [`SecretString`] from the moment they are read out of the environment until
//! [`Delivery::merged_over`] builds the map that goes into the request body, which is the one
//! place a value is exposed and the one place to look for a leak.

use crate::environment::{self, ResolvedVariables};
use secrecy::SecretString;
use std::collections::BTreeMap;

/// The variables one delivery sets on a deployed function.
///
/// Ordered by name, so a description and a request body list them the same way every run.
#[derive(Debug, Default)]
pub struct Delivery(BTreeMap<String, SecretString>);

impl Delivery {
    /// Every runtime variable the deployment resolved.
    #[must_use]
    pub fn from_resolved(resolved: &ResolvedVariables) -> Self {
        Self(
            resolved
                .iter()
                .map(|(name, value)| (name.to_string(), environment::duplicate(value)))
                .collect(),
        )
    }

    /// The same, with a plaintext table underneath it.
    ///
    /// A name the resolved variables also carry keeps its resolved value: `[aws.env]` is
    /// documented as the *non-secret* table, so a name that is also a `[[secret]]` or a wiring
    /// variable is one the deployment supplies a real value for, and shipping the manifest's copy
    /// instead would deploy a placeholder.
    #[must_use]
    pub fn with_defaults(mut self, plaintext: &BTreeMap<String, String>) -> Self {
        for (name, value) in plaintext {
            self.0
                .entry(name.clone())
                .or_insert_with(|| SecretString::from(value.clone()));
        }
        self
    }

    /// One name and value, for `skyzen secret set`.
    #[must_use]
    pub fn one(name: &str, value: &SecretString) -> Self {
        Self(BTreeMap::from([(
            name.to_owned(),
            environment::duplicate(value),
        )]))
    }

    /// Whether there is nothing to deliver.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The names alone, which is all a description or a log line may carry.
    #[must_use]
    pub fn names(&self) -> String {
        self.0
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// This delivery laid over what the deployment already has.
    ///
    /// A read-modify-write rather than a replace, because both platforms' write calls replace the
    /// whole map: a variable another tool set — a Functions host setting, a value from the console
    /// — would otherwise be deleted by a deploy that never mentioned it.
    ///
    /// The one place a delivered value is exposed: what comes back goes into the request body and
    /// nowhere else.
    #[must_use]
    pub fn merged_over(
        &self,
        existing: impl IntoIterator<Item = (String, String)>,
    ) -> BTreeMap<String, String> {
        let mut merged: BTreeMap<String, String> = existing.into_iter().collect();
        merged.extend(
            self.0
                .iter()
                .map(|(name, value)| (name.clone(), environment::expose(value).to_owned())),
        );
        merged
    }
}

#[cfg(test)]
mod tests {
    use super::Delivery;
    use secrecy::SecretString;
    use std::collections::BTreeMap;

    #[test]
    fn a_delivery_keeps_what_is_already_there_and_wins_where_they_collide() {
        let delivery = Delivery::one("STRIPE_KEY", &SecretString::from("sk_live_123"));
        let merged = delivery.merged_over([
            ("RUST_LOG".to_owned(), "info".to_owned()),
            ("STRIPE_KEY".to_owned(), "stale".to_owned()),
        ]);

        // The Functions host's own settings, and anything set from a console, survive a deploy
        // that never mentioned them.
        assert_eq!(merged.get("RUST_LOG").map(String::as_str), Some("info"));
        assert_eq!(
            merged.get("STRIPE_KEY").map(String::as_str),
            Some("sk_live_123")
        );
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn a_resolved_value_beats_the_manifests_plaintext_copy() {
        let resolved = crate::environment::from_dotenv("STRIPE_KEY=sk_live_123\n")
            .resolve(&[crate::environment::RuntimeVariable {
                name: skyzen_manifest::VarName::try_from("STRIPE_KEY".to_owned()).expect("a name"),
                declared_by: "[[secret]] STRIPE_KEY".to_owned(),
                kind: crate::environment::VariableKind::Secret,
            }])
            .expect("the dotenv entry supplies it");

        let delivery = Delivery::from_resolved(&resolved).with_defaults(&BTreeMap::from([
            ("RUST_LOG".to_owned(), "info".to_owned()),
            ("STRIPE_KEY".to_owned(), "placeholder".to_owned()),
        ]));
        assert_eq!(delivery.names(), "RUST_LOG, STRIPE_KEY");

        let merged = delivery.merged_over(BTreeMap::new());
        assert_eq!(merged.get("RUST_LOG").map(String::as_str), Some("info"));
        assert_eq!(
            merged.get("STRIPE_KEY").map(String::as_str),
            Some("sk_live_123")
        );
    }

    #[test]
    fn nothing_declared_is_nothing_to_deliver() {
        assert!(Delivery::default().is_empty());
        assert_eq!(Delivery::default().names(), "");
    }
}
