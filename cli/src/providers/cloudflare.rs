use crate::{
    args::Action,
    manifest::{
        CfD1Database, CfDurableBinding, CfKvNamespace, CfQueueConsumer, CfQueueProducer,
        CfR2Bucket, CloudflareSection, LoadedManifest,
    },
    providers::{CommandPlan, GeneratedFile, ProviderPlan},
};
use anyhow::{Context, Result};
use std::path::PathBuf;

pub fn prepare(action: Action, manifest: &LoadedManifest) -> Result<ProviderPlan> {
    let config = manifest
        .data
        .cloudflare
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("missing [cloudflare] section in Skyzen.toml"))?;
    let wrangler = render_wrangler(config, &manifest.root_dir)?;
    let wrangler_path = manifest.root_dir.join(".skyzen/gen/wrangler.toml");

    let command = match action {
        Action::Dev => CommandPlan {
            program: "wrangler".to_owned(),
            args: vec![
                "dev".to_owned(),
                "--local".to_owned(),
                "--config".to_owned(),
                path_string(&wrangler_path)?,
            ],
            cwd: Some(manifest.root_dir.clone()),
        },
        Action::Deploy => CommandPlan {
            program: "wrangler".to_owned(),
            args: vec![
                "deploy".to_owned(),
                "--config".to_owned(),
                path_string(&wrangler_path)?,
            ],
            cwd: Some(manifest.root_dir.clone()),
        },
        Action::Doctor => unreachable!("doctor is handled in providers::prepare"),
    };

    Ok(ProviderPlan {
        commands: vec![command],
        generated_files: vec![GeneratedFile {
            path: wrangler_path,
            contents: wrangler,
        }],
    })
}

fn render_wrangler(config: &CloudflareSection, root_dir: &PathBuf) -> Result<String> {
    let name = config.name.clone().unwrap_or_else(|| {
        root_dir
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("skyzen-app")
            .to_owned()
    });
    let compatibility_date = config
        .compatibility_date
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "cloudflare.compatibility_date is required (example: compatibility_date = \"2025-02-01\")"
            )
        })?;
    let main = config.main.as_deref().unwrap_or("worker.js");

    let mut out = String::new();
    out.push_str(&format!("name = {}\n", quoted(&name)));
    out.push_str(&format!("main = {}\n", quoted(main)));
    out.push_str(&format!(
        "compatibility_date = {}\n",
        quoted(compatibility_date)
    ));

    if !config.compatibility_flags.is_empty() {
        out.push_str("compatibility_flags = [");
        for (index, flag) in config.compatibility_flags.iter().enumerate() {
            if index > 0 {
                out.push_str(", ");
            }
            out.push_str(&quoted(flag));
        }
        out.push_str("]\n");
    }
    if let Some(account_id) = &config.account_id {
        out.push_str(&format!("account_id = {}\n", quoted(account_id)));
    }
    if let Some(workers_dev) = config.workers_dev {
        out.push_str(&format!("workers_dev = {workers_dev}\n"));
    }
    if let Some(route) = &config.route {
        out.push_str(&format!("route = {}\n", quoted(route)));
    }
    if let Some(zone_id) = &config.zone_id {
        out.push_str(&format!("zone_id = {}\n", quoted(zone_id)));
    }

    append_vars(&mut out, &config.vars);
    append_kv(&mut out, &config.kv_namespaces);
    append_r2(&mut out, &config.r2_buckets);
    append_d1(&mut out, &config.d1_databases);
    append_queue_producers(&mut out, &config.queues.producers);
    append_queue_consumers(&mut out, &config.queues.consumers);
    append_durable_bindings(&mut out, &config.durable_objects.bindings);
    Ok(out)
}

fn append_vars(out: &mut String, vars: &std::collections::BTreeMap<String, String>) {
    if vars.is_empty() {
        return;
    }
    out.push_str("\n[vars]\n");
    for (key, value) in vars {
        out.push_str(&format!("{key} = {}\n", quoted(value)));
    }
}

fn append_kv(out: &mut String, entries: &[CfKvNamespace]) {
    for entry in entries {
        out.push_str("\n[[kv_namespaces]]\n");
        out.push_str(&format!("binding = {}\n", quoted(&entry.binding)));
        out.push_str(&format!("id = {}\n", quoted(&entry.id)));
        if let Some(preview_id) = &entry.preview_id {
            out.push_str(&format!("preview_id = {}\n", quoted(preview_id)));
        }
    }
}

fn append_r2(out: &mut String, entries: &[CfR2Bucket]) {
    for entry in entries {
        out.push_str("\n[[r2_buckets]]\n");
        out.push_str(&format!("binding = {}\n", quoted(&entry.binding)));
        out.push_str(&format!("bucket_name = {}\n", quoted(&entry.bucket_name)));
        if let Some(preview_bucket_name) = &entry.preview_bucket_name {
            out.push_str(&format!(
                "preview_bucket_name = {}\n",
                quoted(preview_bucket_name)
            ));
        }
    }
}

fn append_d1(out: &mut String, entries: &[CfD1Database]) {
    for entry in entries {
        out.push_str("\n[[d1_databases]]\n");
        out.push_str(&format!("binding = {}\n", quoted(&entry.binding)));
        out.push_str(&format!(
            "database_name = {}\n",
            quoted(&entry.database_name)
        ));
        out.push_str(&format!("database_id = {}\n", quoted(&entry.database_id)));
        if let Some(preview_database_id) = &entry.preview_database_id {
            out.push_str(&format!(
                "preview_database_id = {}\n",
                quoted(preview_database_id)
            ));
        }
    }
}

fn append_queue_producers(out: &mut String, entries: &[CfQueueProducer]) {
    for entry in entries {
        out.push_str("\n[[queues.producers]]\n");
        out.push_str(&format!("binding = {}\n", quoted(&entry.binding)));
        out.push_str(&format!("queue = {}\n", quoted(&entry.queue)));
    }
}

fn append_queue_consumers(out: &mut String, entries: &[CfQueueConsumer]) {
    for entry in entries {
        out.push_str("\n[[queues.consumers]]\n");
        out.push_str(&format!("queue = {}\n", quoted(&entry.queue)));
    }
}

fn append_durable_bindings(out: &mut String, entries: &[CfDurableBinding]) {
    for entry in entries {
        out.push_str("\n[[durable_objects.bindings]]\n");
        out.push_str(&format!("name = {}\n", quoted(&entry.name)));
        out.push_str(&format!("class_name = {}\n", quoted(&entry.class_name)));
    }
}

fn quoted(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('\"', "\\\""))
}

fn path_string(path: &PathBuf) -> Result<String> {
    path.to_str()
        .map(ToOwned::to_owned)
        .with_context(|| format!("path is not valid UTF-8: {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{CfDurableObjects, CfQueues};
    use std::collections::BTreeMap;

    #[test]
    fn renders_wrangler_with_bindings() {
        let mut vars = BTreeMap::new();
        vars.insert("APP_ENV".to_owned(), "dev".to_owned());

        let section = CloudflareSection {
            name: Some("skyzen-worker".to_owned()),
            main: Some("dist/worker.js".to_owned()),
            compatibility_date: Some("2025-02-01".to_owned()),
            compatibility_flags: vec!["nodejs_compat".to_owned()],
            account_id: Some("abc".to_owned()),
            workers_dev: Some(true),
            route: None,
            zone_id: None,
            vars,
            kv_namespaces: vec![CfKvNamespace {
                binding: "CACHE".to_owned(),
                id: "123".to_owned(),
                preview_id: Some("456".to_owned()),
            }],
            r2_buckets: vec![],
            d1_databases: vec![CfD1Database {
                binding: "DB".to_owned(),
                database_name: "app".to_owned(),
                database_id: "d1-id".to_owned(),
                preview_database_id: None,
            }],
            queues: CfQueues {
                producers: vec![CfQueueProducer {
                    binding: "JOBS".to_owned(),
                    queue: "jobs".to_owned(),
                }],
                consumers: vec![],
            },
            durable_objects: CfDurableObjects {
                bindings: vec![CfDurableBinding {
                    name: "STATE".to_owned(),
                    class_name: "State".to_owned(),
                }],
            },
        };

        let rendered = render_wrangler(&section, &PathBuf::from("/tmp/app")).expect("render");
        assert!(rendered.contains("name = \"skyzen-worker\""));
        assert!(rendered.contains("[[d1_databases]]"));
        assert!(rendered.contains("binding = \"DB\""));
        assert!(rendered.contains("[[durable_objects.bindings]]"));
        assert!(rendered.contains("[vars]"));
    }
}
