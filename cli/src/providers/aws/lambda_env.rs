//! Delivering a Lambda function's environment, in process.
//!
//! `cargo lambda deploy` can set environment variables, but only as `--env-var NAME=value` on its
//! own command line — which is printed by every progress line and every `--dry-run`, and visible
//! in the process table of whatever machine runs the deploy. So Skyzen does not ask it to: the
//! upload carries the code, and the environment is delivered straight afterwards through
//! `UpdateFunctionConfiguration`, where the value only ever exists inside the request body.
//!
//! The call is a read-modify-write. `UpdateFunctionConfiguration` replaces the whole map, so a
//! variable set from the console or by another tool would be deleted by a deploy that never
//! mentioned it.

use crate::{
    output,
    providers::{secrets::Delivery, Task},
    runtime::block_on,
};
use anyhow::{Context, Result};
use aws_sdk_lambda::{client::Waiters as _, types::Environment, Client};
use std::{collections::BTreeMap, time::Duration};

/// How long to wait for an update already in flight to finish.
///
/// `cargo lambda deploy` returns as soon as Lambda has accepted the code, while the function's
/// `LastUpdateStatus` is still `InProgress`; a second update during that window is rejected with a
/// `ResourceConflictException`. Five minutes is the SDK's own default for this waiter.
const UPDATE_TIMEOUT: Duration = Duration::from_secs(300);

/// Set a deployed function's environment to what the manifest and the local `.env` say.
#[derive(Debug)]
pub struct LambdaEnvironment {
    /// The function to update, as `deploy` names it.
    function: String,
    /// What to deliver.
    delivery: Delivery,
}

impl LambdaEnvironment {
    /// The step that delivers `delivery` to `function`.
    pub const fn new(function: String, delivery: Delivery) -> Self {
        Self { function, delivery }
    }
}

impl Task for LambdaEnvironment {
    fn describe(&self) -> String {
        format!(
            "set the environment of Lambda function `{}`: {}",
            self.function,
            self.delivery.names()
        )
    }

    fn run(&self) -> Result<()> {
        block_on(async {
            let client = client().await;

            // The deploy that just ran leaves an update in progress, and Lambda refuses a second
            // one until it has settled.
            client
                .wait_until_function_updated_v2()
                .function_name(&self.function)
                .wait(UPDATE_TIMEOUT)
                .await
                .with_context(|| {
                    format!(
                        "Lambda function `{}` did not finish updating within {} seconds",
                        self.function,
                        UPDATE_TIMEOUT.as_secs()
                    )
                })?;

            let merged = self
                .delivery
                .merged_over(existing_variables(&client, &self.function).await?);
            client
                .update_function_configuration()
                .function_name(&self.function)
                .environment(
                    Environment::builder()
                        .set_variables(Some(merged.into_iter().collect()))
                        .build(),
                )
                .send()
                .await
                .with_context(|| {
                    format!(
                        "failed to set the environment of Lambda function `{}`",
                        self.function
                    )
                })?;
            Ok(())
        })?
    }
}

/// Report the environment variable names a deployed function carries.
///
/// Names only: `GetFunctionConfiguration` hands back the values as well, and printing one would
/// put a delivered secret on a terminal.
#[derive(Debug)]
pub struct LambdaEnvironmentNames {
    /// The function to read.
    function: String,
}

impl LambdaEnvironmentNames {
    /// The step that lists `function`'s environment.
    pub const fn new(function: String) -> Self {
        Self { function }
    }
}

impl Task for LambdaEnvironmentNames {
    fn describe(&self) -> String {
        format!(
            "list the environment of Lambda function `{}`",
            self.function
        )
    }

    fn run(&self) -> Result<()> {
        block_on(async {
            let client = client().await;
            let variables = existing_variables(&client, &self.function).await?;
            if variables.is_empty() {
                output::ok(format!(
                    "Lambda function `{}` has no environment variables",
                    self.function
                ));
                return Ok(());
            }
            for name in variables.keys() {
                output::ok(name);
            }
            Ok(())
        })?
    }
}

/// A Lambda client on the ambient AWS configuration.
///
/// The same credential chain `cargo lambda deploy` itself resolves — profile, environment,
/// instance role — so the two halves of one deploy cannot end up talking to different accounts.
async fn client() -> Client {
    Client::new(&aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await)
}

/// The environment the deployed function already has.
///
/// # Errors
///
/// Fails when the function cannot be read, which for `secret push` usually means it has not been
/// deployed yet.
async fn existing_variables(client: &Client, function: &str) -> Result<BTreeMap<String, String>> {
    let configuration = client
        .get_function_configuration()
        .function_name(function)
        .send()
        .await
        .with_context(|| {
            format!("failed to read the configuration of Lambda function `{function}`")
        })?;
    Ok(configuration
        .environment
        .and_then(|environment| environment.variables)
        .unwrap_or_default()
        .into_iter()
        .collect())
}
