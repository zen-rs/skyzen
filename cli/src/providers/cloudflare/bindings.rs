//! Give each application its own wasm-bindgen module state and count its live
//! JS→wasm transitions.
//!
//! The pinned web generator emits one-line imports and export declarations. Keep
//! imports at module scope and put its mutable glue, classes, and Wasm instance
//! in a factory. This is a build-time transform, not runtime eval. The factory
//! also wraps the instance's exports with a depth counter so the worker shim can
//! tell a terminated invocation (which never ran its `finally`) apart from a
//! suspended one.

use anyhow::{bail, Context, Result};
use regex::Regex;
use std::fmt::Write as _;

pub(super) fn invocation_factory(source: &str) -> Result<String> {
    let declaration = Regex::new(r"^(?:async )?(?:function|class|const|let|var) ([\w$]+)")?;
    let mut imports = String::new();
    let mut body = String::new();
    let mut exports = Vec::new();
    for line in source.lines() {
        if line.starts_with("import ") {
            if !line.ends_with(';') {
                bail!("unsupported wasm-bindgen import: {line}");
            }
            writeln!(imports, "{line}")?;
        } else if let Some(rest) = line.strip_prefix("export ") {
            if let Some(list) = rest.strip_prefix("{ ") {
                let list = list
                    .strip_suffix(" };")
                    .or_else(|| list.strip_suffix(" }"))
                    .with_context(|| format!("unsupported wasm-bindgen exports: {line}"))?;
                for item in list.split(',') {
                    let item = item.trim();
                    let (local, public) = item.split_once(" as ").unwrap_or((item, item));
                    exports.push((local.to_owned(), public.to_owned()));
                }
            } else {
                let capture = declaration
                    .captures(rest)
                    .with_context(|| format!("unsupported wasm-bindgen export: {line}"))?;
                let name = capture[1].to_owned();
                exports.push((name.clone(), name));
                writeln!(body, "{rest}")?;
            }
        } else {
            writeln!(body, "{line}")?;
        }
    }
    if !exports.iter().any(|(local, _)| local == "initSync") {
        bail!("wasm-bindgen web bindings must export initSync");
    }
    let mut output = format!(
        "{imports}\nexport function createBindings(module, onError = () => {{}}) {{\n\
         // Each executor callback belongs to this instance, including its errors.\n\
         // A posted callback decrements back to zero before the next event\n\
         // runs; a positive count at an event entry means a queued callback\n\
         // never ran — dropped, or killed while pending — which strands the\n\
         // wasm-bindgen executor's scheduling flag, so it is poison.\n\
     let stranded = 0;\n\
     const queueMicrotask = callback => {{\n\
         stranded += 1;\n\
         globalThis.queueMicrotask(() => {{\n\
             stranded -= 1;\n\
             try {{ callback(); }} catch (error) {{ onError(error); throw error; }}\n\
         }});\n\
     }};\n{body}\ninitSync({{ module }});\n\
     // Every JS→wasm transition goes through this proxy: plain export calls,\n\
     // wasm-bindgen closure invocations and the executor's queued microtasks\n\
     // alike resolve `wasm.<name>` here. `depth` is the number of calls still\n\
     // inside wasm. A V8 termination (the CPU limit) is uncatchable and skips\n\
     // `finally`, so it leaves `depth` above zero — the worker shim reads it\n\
     // at each event entry to detect a poisoned application.\n\
     // The exports object is a frozen module namespace, so a Proxy cannot\n\
     // return wrapped functions for it (non-configurable data properties).\n\
     // Copy it instead: function exports get the depth-counting wrapper,\n\
     // memory, tables and globals are shared by reference.\n\
     const live = wasm;\n\
     let depth = 0;\n\
     const copy = {{}};\n\
     for (const prop of Object.keys(live)) {{\n\
         const value = live[prop];\n\
         copy[prop] = typeof value === \"function\" ? (...args) => {{\n\
             depth += 1;\n\
             try {{\n\
                 return Reflect.apply(value, live, args);\n\
             }} finally {{\n\
                 depth -= 1;\n\
             }}\n\
         }} : value;\n\
     }}\n\
     wasm = copy;\nreturn {{\nget depth() {{ return depth; }},\nget stranded() {{ return stranded; }},\n"
    );
    for (local, public) in exports {
        // Names, including quoted JS export names, are emitted by wasm-bindgen.
        writeln!(output, "get {public}() {{ return {local}; }},")?;
    }
    output.push_str("};\n}\n");
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::invocation_factory;

    #[test]
    fn scopes_glue_and_preserves_imports_and_export_aliases() {
        let source = "import { foo } from './snippet.js';\n\
            let wasm;\n\
            export class Room {}\n\
            export function fetch() { return wasm; }\n\
            function initSync({ module }) { wasm = module; }\n\
            export { initSync, fetch as 'unusual-name', initSync as default };\n";
        let result = invocation_factory(source).unwrap();
        assert!(result.starts_with("import { foo } from './snippet.js';\n"));
        assert!(result.contains("get 'unusual-name'() { return fetch; }"));
        assert!(result.contains("get Room() { return Room; }"));
        assert!(result.contains("get default() { return initSync; }"));
        assert!(result.contains("initSync({ module });"));
        assert!(!result.contains("export class"));
    }

    #[test]
    fn callbacks_keep_their_instance_after_another_is_created() {
        let source = "let wasm;\n\
            function initSync({ module }) { wasm = { count: module }; }\n\
            export function callback() { return () => ++wasm.count; }\n\
            export { initSync };\n";
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("bindings.mjs"),
            invocation_factory(source).unwrap(),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("test.mjs"),
            r"
            import assert from 'node:assert/strict';
            import { createBindings } from './bindings.mjs';
            const a = createBindings(0).callback();
            const b = createBindings(100).callback();
            assert.equal(a(), 1);
            assert.equal(b(), 101);
            assert.equal(a(), 2);
        ",
        )
        .unwrap();
        let output = std::process::Command::new("node")
            .arg(dir.path().join("test.mjs"))
            .output()
            .expect("Node.js is required to test the generated bindings");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn fails_closed_on_unrecognized_generator_output() {
        for source in [
            "export * from './other.js';",
            "import {\n foo\n} from './other.js';",
            "export function fetch() {}",
        ] {
            assert!(invocation_factory(source).is_err());
        }
    }
}
