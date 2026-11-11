//! Process environment variables a test sets for the AWS SDK's providers to read.

use std::{ffi::OsString, path::Path};

/// Sets process environment variables and restores them on drop.
///
/// The variables are the process's, so every test using this is `#[serial]`.
pub struct Environment(Vec<(&'static str, Option<OsString>)>);

impl Environment {
    /// Set each variable to its value, or remove it where the value is `None`.
    pub fn set(vars: &[(&'static str, Option<&Path>)]) -> Self {
        let saved = vars
            .iter()
            .map(|(name, _)| (*name, std::env::var_os(name)))
            .collect();
        for (name, value) in vars {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
        Self(saved)
    }
}

impl Drop for Environment {
    fn drop(&mut self) {
        for (name, value) in self.0.drain(..) {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}
