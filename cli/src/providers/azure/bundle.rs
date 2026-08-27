//! The Functions bundle: `host.json`, and one `function.json` per function.
//!
//! Rendered from typed structs through `serde_json` rather than assembled as text, so a key that
//! is renamed or dropped fails the build instead of producing a bundle the host silently ignores.

use crate::providers::GeneratedFile;
use anyhow::{Context, Result};
use serde::Serialize;
use skyzen_manifest::{AzureQueueTrigger, AzureSection, HTTP_FUNCTION_NAME};
use std::{collections::BTreeMap, path::Path};

/// The `function.json` schema version every Functions app declares.
const HOST_VERSION: &str = "2.0";

/// The extension bundle that supplies the queue trigger.
///
/// Version 4 is the current major, and the first that honours `messageEncoding`.
const EXTENSION_BUNDLE_ID: &str = "Microsoft.Azure.Functions.ExtensionBundle";

/// The version range for that bundle: the latest 4.x, resolved by the host.
const EXTENSION_BUNDLE_VERSION: &str = "[4.0.0, 5.0.0)";

/// Render every file the bundle needs.
///
/// # Errors
///
/// Fails when a rendered document cannot be serialized, which would mean the structs below no
/// longer describe JSON.
pub fn render(
    config: &AzureSection,
    binary: &str,
    bundle_dir: &Path,
) -> Result<Vec<GeneratedFile>> {
    let mut files = vec![
        GeneratedFile {
            path: bundle_dir.join("host.json"),
            contents: to_json(&HostJson::new(config, binary))?,
        },
        GeneratedFile {
            path: bundle_dir.join("local.settings.json"),
            contents: to_json(&LocalSettings::default())?,
        },
        GeneratedFile {
            // Every HTTP request arrives through this one function: its route is a wildcard and
            // it accepts every method, so routing stays the application's own router's job. The
            // name is reserved in the schema, so no queue trigger can take this directory.
            path: bundle_dir.join(HTTP_FUNCTION_NAME).join("function.json"),
            contents: to_json(&FunctionJson::http())?,
        },
    ];

    for trigger in &config.queue_triggers {
        files.push(GeneratedFile {
            path: bundle_dir.join(&trigger.function).join("function.json"),
            contents: to_json(&FunctionJson::queue(trigger))?,
        });
    }

    Ok(files)
}

/// Serialize one document, with the trailing newline every generated file gets.
fn to_json<T: Serialize>(value: &T) -> Result<String> {
    let mut rendered =
        serde_json::to_string_pretty(value).context("failed to render a Functions bundle file")?;
    rendered.push('\n');
    Ok(rendered)
}

/// `host.json` — how the host starts the handler and how it treats the triggers.
#[derive(Debug, Serialize)]
struct HostJson<'a> {
    version: &'static str,
    #[serde(rename = "customHandler")]
    custom_handler: CustomHandler<'a>,
    #[serde(rename = "extensionBundle")]
    extension_bundle: ExtensionBundle,
    extensions: Extensions,
}

impl<'a> HostJson<'a> {
    fn new(config: &AzureSection, binary: &'a str) -> Self {
        Self {
            version: HOST_VERSION,
            custom_handler: CustomHandler {
                description: HandlerDescription {
                    default_executable_path: binary,
                },
                // Exactly one of the two forwarding keys, named by the manifest: the host ignores
                // the one it is not given, and setting both would leave which wins to the host.
                forwarding: BTreeMap::from([(config.http_mode.host_json_key(), true)]),
            },
            extension_bundle: ExtensionBundle {
                id: EXTENSION_BUNDLE_ID,
                version: EXTENSION_BUNDLE_VERSION,
            },
            extensions: Extensions {
                http: HttpExtension {
                    // Emptied so the path the host forwards is the path the client asked for.
                    // With the default `api` prefix, an application route `/greet` would only ever
                    // be reachable as `/api/greet` — and the router would never see it.
                    route_prefix: "",
                },
                queues: QueuesExtension {
                    // Skyzen's Storage queue client writes plain text with its own in-band
                    // encoding tag. The host's default is to base64-decode every message first,
                    // which would corrupt each one before the handler saw it.
                    message_encoding: "none",
                },
            },
        }
    }
}

/// The `customHandler` section.
#[derive(Debug, Serialize)]
struct CustomHandler<'a> {
    description: HandlerDescription<'a>,
    /// `enableForwardingHttpRequest` or `enableProxyingHttpRequest`, whichever the manifest asked
    /// for. A map because the *key* is the choice.
    #[serde(flatten)]
    forwarding: BTreeMap<&'static str, bool>,
}

/// How to start the handler process.
#[derive(Debug, Serialize)]
struct HandlerDescription<'a> {
    /// Relative to the app root, which is the bundle directory the binary is staged into.
    #[serde(rename = "defaultExecutablePath")]
    default_executable_path: &'a str,
}

/// The extension bundle reference, without which a queue trigger has no implementation.
#[derive(Debug, Serialize)]
struct ExtensionBundle {
    id: &'static str,
    version: &'static str,
}

/// Per-extension settings.
#[derive(Debug, Serialize)]
struct Extensions {
    http: HttpExtension,
    queues: QueuesExtension,
}

/// `extensions.http`.
#[derive(Debug, Serialize)]
struct HttpExtension {
    #[serde(rename = "routePrefix")]
    route_prefix: &'static str,
}

/// `extensions.queues`.
#[derive(Debug, Serialize)]
struct QueuesExtension {
    #[serde(rename = "messageEncoding")]
    message_encoding: &'static str,
}

/// `local.settings.json` — what `func start` reads when running the bundle locally.
///
/// It carries no secrets: the storage connection a queue trigger needs is an app setting in Azure,
/// and locally it is whatever the developer puts here themselves.
#[derive(Debug, Serialize)]
struct LocalSettings {
    #[serde(rename = "IsEncrypted")]
    is_encrypted: bool,
    #[serde(rename = "Values")]
    values: BTreeMap<&'static str, &'static str>,
}

impl Default for LocalSettings {
    fn default() -> Self {
        Self {
            is_encrypted: false,
            values: BTreeMap::from([("FUNCTIONS_WORKER_RUNTIME", "Custom")]),
        }
    }
}

/// One function's `function.json`.
#[derive(Debug, Serialize)]
struct FunctionJson {
    bindings: Vec<Binding>,
}

impl FunctionJson {
    /// The catch-all HTTP function: every method, every path.
    fn http() -> Self {
        Self {
            bindings: vec![
                Binding {
                    kind: "httpTrigger",
                    direction: "in",
                    name: "req",
                    // Anonymous because the application is the thing deciding who may call it; a
                    // function key in front of a router would apply to every route at once.
                    auth_level: Some("anonymous"),
                    // The wildcard is what makes one function serve the whole application.
                    route: Some("{*path}".to_owned()),
                    methods: Some(
                        [
                            "get", "post", "put", "patch", "delete", "head", "options", "trace",
                        ]
                        .map(ToOwned::to_owned)
                        .to_vec(),
                    ),
                    queue_name: None,
                    connection: None,
                },
                Binding {
                    kind: "http",
                    direction: "out",
                    name: "res",
                    auth_level: None,
                    route: None,
                    methods: None,
                    queue_name: None,
                    connection: None,
                },
            ],
        }
    }

    /// One Storage queue trigger.
    ///
    /// The binding is the function's only input, which is what lets the runtime read the message
    /// out of the envelope without the two sides having to agree on a name.
    fn queue(trigger: &AzureQueueTrigger) -> Self {
        Self {
            bindings: vec![Binding {
                kind: "queueTrigger",
                direction: "in",
                name: "message",
                auth_level: None,
                route: None,
                methods: None,
                queue_name: Some(trigger.queue.clone()),
                connection: Some(trigger.connection_env.clone()),
            }],
        }
    }
}

/// One entry of a function's `bindings` array.
///
/// One struct for every binding kind, with the keys a kind does not use left out of the JSON: the
/// alternative is an enum whose variants differ by two fields each, and the host reads this as an
/// untagged bag of keys anyway.
#[derive(Debug, Serialize)]
struct Binding {
    #[serde(rename = "type")]
    kind: &'static str,
    direction: &'static str,
    name: &'static str,
    #[serde(rename = "authLevel", skip_serializing_if = "Option::is_none")]
    auth_level: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    route: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    methods: Option<Vec<String>>,
    #[serde(rename = "queueName", skip_serializing_if = "Option::is_none")]
    queue_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    connection: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::render;
    use skyzen_manifest::Manifest;
    use std::path::{Path, PathBuf};

    fn config(source: &str) -> skyzen_manifest::AzureSection {
        Manifest::parse(source, "Skyzen.toml", "/tmp/app")
            .expect("valid manifest")
            .data()
            .azure
            .clone()
            .unwrap_or_default()
    }

    fn rendered(source: &str) -> Vec<(PathBuf, serde_json::Value)> {
        render(
            &config(source),
            "demo",
            Path::new("/tmp/app/.skyzen/gen/azure"),
        )
        .expect("the bundle renders")
        .into_iter()
        .map(|file| {
            let parsed = serde_json::from_str(&file.contents)
                .unwrap_or_else(|error| panic!("{} is not JSON: {error}", file.path.display()));
            (file.path, parsed)
        })
        .collect()
    }

    fn document<'a>(
        files: &'a [(PathBuf, serde_json::Value)],
        name: &str,
    ) -> &'a serde_json::Value {
        // `Path::ends_with` compares whole components, so a `/`-separated `name` matches the
        // platform separator on Windows too — a string `contains` would not.
        &files
            .iter()
            .find(|(path, _)| path.ends_with(name))
            .unwrap_or_else(|| panic!("no {name} in the bundle"))
            .1
    }

    #[test]
    fn host_json_starts_the_staged_binary_and_forwards_http_to_it() {
        let files = rendered("[azure]\napp_name = \"skyzen-demo\"\n");
        let host = document(&files, "host.json");

        assert_eq!(host["version"], "2.0");
        assert_eq!(
            host["customHandler"]["description"]["defaultExecutablePath"],
            "demo"
        );
        assert_eq!(host["customHandler"]["enableForwardingHttpRequest"], true);
        assert!(host["customHandler"]["enableProxyingHttpRequest"].is_null());
    }

    #[test]
    fn the_proxying_mode_names_the_other_key_and_only_that_one() {
        let files = rendered("[azure]\nhttp_mode = \"proxy\"\n");
        let host = document(&files, "host.json");

        assert_eq!(host["customHandler"]["enableProxyingHttpRequest"], true);
        assert!(host["customHandler"]["enableForwardingHttpRequest"].is_null());
    }

    #[test]
    fn host_json_clears_the_route_prefix_so_the_router_sees_the_real_path() {
        let host = rendered("[azure]\n");
        let host = document(&host, "host.json");

        // Without this the host would forward `/api/greet` for a route registered as `/greet`.
        assert_eq!(host["extensions"]["http"]["routePrefix"], "");
    }

    #[test]
    fn host_json_stops_the_queue_extension_from_base64_decoding_every_message() {
        let files = rendered("[azure]\n");
        let host = document(&files, "host.json");

        assert_eq!(host["extensions"]["queues"]["messageEncoding"], "none");
        assert_eq!(
            host["extensionBundle"]["id"],
            "Microsoft.Azure.Functions.ExtensionBundle"
        );
    }

    #[test]
    fn the_http_function_is_one_anonymous_catch_all() {
        let files = rendered("[azure]\n");
        let http = document(&files, "http/function.json");
        let trigger = &http["bindings"][0];

        assert_eq!(trigger["type"], "httpTrigger");
        assert_eq!(trigger["route"], "{*path}");
        assert_eq!(trigger["authLevel"], "anonymous");
        assert_eq!(trigger["methods"][0], "get");
        assert_eq!(trigger["methods"].as_array().expect("methods").len(), 8);
        assert_eq!(http["bindings"][1]["type"], "http");
        assert_eq!(http["bindings"][1]["direction"], "out");
    }

    #[test]
    fn a_queue_trigger_gets_its_own_function_directory() {
        let files = rendered(
            "[azure]\n\n[[azure.queue_triggers]]\nfunction = \"process\"\nqueue = \"jobs\"\n\
             connection_env = \"AzureWebJobsStorage\"\n",
        );

        let (path, _) = files
            .iter()
            .find(|(path, _)| path.ends_with("process/function.json"))
            .expect("the trigger has a directory of its own");
        // Component-wise so the platform separator on Windows matches too.
        assert!(path.ends_with(".skyzen/gen/azure/process/function.json"));

        let queue = document(&files, "process/function.json");
        let binding = &queue["bindings"][0];
        assert_eq!(binding["type"], "queueTrigger");
        assert_eq!(binding["direction"], "in");
        assert_eq!(binding["queueName"], "jobs");
        assert_eq!(binding["connection"], "AzureWebJobsStorage");
        // Exactly one binding, which is what lets the runtime read the message without agreeing
        // on its name.
        assert_eq!(queue["bindings"].as_array().expect("bindings").len(), 1);
    }

    #[test]
    fn local_settings_declare_the_custom_handler_runtime() {
        let files = rendered("[azure]\n");
        let settings = document(&files, "local.settings.json");

        assert_eq!(settings["Values"]["FUNCTIONS_WORKER_RUNTIME"], "Custom");
        assert_eq!(settings["IsEncrypted"], false);
    }
}
