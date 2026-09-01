//! Delivering a Function App's application settings over ARM.
//!
//! A custom handler reads its configuration from the process environment, and on Functions that
//! environment *is* the app's settings. There is no Rust SDK for the `Microsoft.Web` provider, so
//! this speaks the two REST calls directly: `config/appsettings/list` to read and
//! `config/appsettings` to write. The write is a **full replace**, which is why every path here is
//! a read-modify-write — the host's own `FUNCTIONS_WORKER_RUNTIME` and `AzureWebJobsStorage` live
//! in the same map, and a PUT that did not carry them would take the app down.
//!
//! Authentication is the Azure CLI's own token (`az account get-access-token`), so a deploy uses
//! the identity the developer already logged in with and Skyzen never handles a credential.

use crate::{
    output,
    providers::{secrets::Delivery, Task},
    runtime::block_on,
};
use anyhow::{Context, Result};
use secrecy::{ExposeSecret as _, SecretString};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    process::{Command, Stdio},
};

/// The ARM API version the two settings calls are made against.
const API_VERSION: &str = "2022-03-01";

/// The ARM endpoint, which is also the audience the access token is issued for.
const MANAGEMENT_ENDPOINT: &str = "https://management.azure.com/";

/// The Function App one delivery acts on.
///
/// All three parts are needed to address it: application settings are a child resource of the
/// site, and a site is identified by subscription, resource group and name.
#[derive(Debug, Clone)]
pub struct FunctionApp {
    /// The subscription the app lives in.
    pub subscription_id: String,
    /// The resource group inside it.
    pub resource_group: String,
    /// The Function App's name.
    pub name: String,
}

impl FunctionApp {
    /// The settings resource's URL, for the given verb's path suffix.
    fn url(&self, suffix: &str) -> String {
        format!(
            "{MANAGEMENT_ENDPOINT}subscriptions/{}/resourceGroups/{}/providers/Microsoft.Web/\
             sites/{}/config/appsettings{suffix}?api-version={API_VERSION}",
            self.subscription_id, self.resource_group, self.name
        )
    }
}

/// Set a Function App's application settings to what the manifest and the local `.env` say.
#[derive(Debug)]
pub struct AppSettings {
    /// The app to update.
    app: FunctionApp,
    /// What to deliver.
    delivery: Delivery,
}

impl AppSettings {
    /// The step that delivers `delivery` to `app`.
    pub const fn new(app: FunctionApp, delivery: Delivery) -> Self {
        Self { app, delivery }
    }
}

impl Task for AppSettings {
    fn describe(&self) -> String {
        format!(
            "set the application settings of Function App `{}`: {}",
            self.app.name,
            self.delivery.names()
        )
    }

    fn run(&self) -> Result<()> {
        let token = access_token()?;
        block_on(async {
            let client = reqwest::Client::new();
            let existing = list(&client, &self.app, &token).await?;
            let merged = self.delivery.merged_over(existing);

            // The one place a delivered value is exposed: it goes into this body and nowhere else.
            let response = client
                .put(self.app.url(""))
                .bearer_auth(token.expose_secret())
                .json(&AppSettingsDocument {
                    properties: &merged,
                })
                .send()
                .await
                .context("failed to write the Function App's application settings")?;
            check(response, "write the application settings").await?;
            Ok(())
        })?
    }
}

/// Report the application setting names a Function App carries.
///
/// Names only: the list call hands back the values as well, and printing one would put a delivered
/// secret on a terminal.
#[derive(Debug)]
pub struct AppSettingNames {
    /// The app to read.
    app: FunctionApp,
}

impl AppSettingNames {
    /// The step that lists `app`'s settings.
    pub const fn new(app: FunctionApp) -> Self {
        Self { app }
    }
}

impl Task for AppSettingNames {
    fn describe(&self) -> String {
        format!(
            "list the application settings of Function App `{}`",
            self.app.name
        )
    }

    fn run(&self) -> Result<()> {
        let token = access_token()?;
        block_on(async {
            let settings = list(&reqwest::Client::new(), &self.app, &token).await?;
            if settings.is_empty() {
                output::ok(format!(
                    "Function App `{}` has no application settings",
                    self.app.name
                ));
                return Ok(());
            }
            for name in settings.keys() {
                output::ok(name);
            }
            Ok(())
        })?
    }
}

/// The settings the app already has.
///
/// A POST rather than a GET, because ARM treats reading values as an action rather than a
/// projection of the resource.
///
/// # Errors
///
/// Fails when the request cannot be sent, when ARM refuses it, or when its answer is not the
/// document this expects.
async fn list(
    client: &reqwest::Client,
    app: &FunctionApp,
    token: &SecretString,
) -> Result<BTreeMap<String, String>> {
    let response = client
        .post(app.url("/list"))
        .bearer_auth(token.expose_secret())
        .header(reqwest::header::CONTENT_LENGTH, 0)
        .send()
        .await
        .context("failed to read the Function App's application settings")?;
    let body = check(response, "read the application settings").await?;
    let document: OwnedAppSettingsDocument = serde_json::from_str(&body)
        .context("the application settings ARM returned are not the document Skyzen expects")?;
    Ok(document.properties)
}

/// The body both calls carry, and the answer both of them give.
#[derive(Debug, Serialize)]
struct AppSettingsDocument<'a> {
    /// The whole settings map: ARM replaces what the app has with exactly this.
    properties: &'a BTreeMap<String, String>,
}

/// The same document, read back.
#[derive(Debug, Deserialize)]
struct OwnedAppSettingsDocument {
    /// A Function App with no settings at all still answers, with this absent.
    #[serde(default)]
    properties: BTreeMap<String, String>,
}

/// What ARM says when it refuses.
#[derive(Debug, Deserialize)]
struct ArmErrorDocument {
    /// Absent when the failure came from a layer that does not use ARM's error shape.
    error: Option<ArmError>,
}

/// The error body itself.
#[derive(Debug, Deserialize)]
struct ArmError {
    /// The human-readable reason, which is the one part worth repeating.
    message: Option<String>,
}

/// The response's body, or an error naming the status and what ARM said about it.
///
/// The request body is never echoed: on the write call it is the values themselves.
///
/// # Errors
///
/// Fails for any non-2xx status, and when the body cannot be read.
async fn check(response: reqwest::Response, what: &str) -> Result<String> {
    let status = response.status();
    let body = response
        .text()
        .await
        .with_context(|| format!("failed to read Azure's answer when asked to {what}"))?;
    if status.is_success() {
        return Ok(body);
    }

    let reason = serde_json::from_str::<ArmErrorDocument>(&body)
        .ok()
        .and_then(|document| document.error?.message);
    match reason {
        Some(message) => anyhow::bail!("Azure refused to {what} ({status}): {message}"),
        None => anyhow::bail!("Azure refused to {what} ({status})"),
    }
}

/// A bearer token for ARM, from the Azure CLI's own logged-in identity.
///
/// `func azure functionapp publish` authenticates the same way, so a deploy that can publish can
/// also deliver its settings — there is no second credential to configure.
///
/// # Errors
///
/// Fails when `az` is not installed, when nobody is logged in, or when its answer is not the JSON
/// document it documents.
fn access_token() -> Result<SecretString> {
    let output = Command::new("az")
        .args([
            "account",
            "get-access-token",
            "--resource",
            MANAGEMENT_ENDPOINT,
            "-o",
            "json",
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .context(
            "failed to run `az`; install the Azure CLI (https://aka.ms/azure-cli) and run \
             `az login`",
        )?;
    if !output.status.success() {
        anyhow::bail!(
            "`az account get-access-token` failed with status {}; run `az login`",
            output.status
        );
    }

    let document: AccessTokenDocument = serde_json::from_slice(&output.stdout)
        .context("`az account get-access-token -o json` did not print the document it documents")?;
    // Straight into the wrapper: from here the token only reaches an `Authorization` header.
    Ok(SecretString::from(document.access_token))
}

/// What `az account get-access-token -o json` prints.
#[derive(Debug, Deserialize)]
struct AccessTokenDocument {
    /// The bearer token itself. The other keys — expiry, subscription, tenant — say nothing this
    /// needs, and `deny_unknown_fields` would break on the next one the CLI adds.
    #[serde(rename = "accessToken")]
    access_token: String,
}

#[cfg(test)]
mod tests {
    use super::{AppSettingsDocument, FunctionApp, OwnedAppSettingsDocument};
    use std::collections::BTreeMap;

    fn app() -> FunctionApp {
        FunctionApp {
            subscription_id: "00000000-0000-0000-0000-000000000000".to_owned(),
            resource_group: "skyzen-rg".to_owned(),
            name: "skyzen-demo".to_owned(),
        }
    }

    #[test]
    fn the_two_calls_address_the_same_settings_resource() {
        assert_eq!(
            app().url("/list"),
            "https://management.azure.com/subscriptions/00000000-0000-0000-0000-000000000000/\
             resourceGroups/skyzen-rg/providers/Microsoft.Web/sites/skyzen-demo/config/\
             appsettings/list?api-version=2022-03-01"
        );
        assert!(app().url("").contains("/config/appsettings?api-version="));
    }

    #[test]
    fn the_body_is_the_whole_settings_map_under_properties() {
        let settings = BTreeMap::from([("STRIPE_KEY".to_owned(), "sk_live_123".to_owned())]);
        let rendered = serde_json::to_value(AppSettingsDocument {
            properties: &settings,
        })
        .expect("the body serializes");

        assert_eq!(rendered["properties"]["STRIPE_KEY"], "sk_live_123");
        assert_eq!(
            rendered.as_object().expect("an object").len(),
            1,
            "ARM replaces the whole map, so `properties` is the only key it reads"
        );
    }

    #[test]
    fn an_app_with_no_settings_reads_back_as_an_empty_map() {
        let document: OwnedAppSettingsDocument = serde_json::from_str("{}").expect("parses");
        assert!(document.properties.is_empty());

        let document: OwnedAppSettingsDocument =
            serde_json::from_str(r#"{"id":"/subscriptions/x","properties":{"A":"1"}}"#)
                .expect("parses");
        assert_eq!(document.properties.get("A").map(String::as_str), Some("1"));
    }
}
