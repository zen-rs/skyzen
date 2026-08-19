import init, * as wasmExports from "./__SKYZEN_BINDINGS_JS__";
import wasmUrl from "./__SKYZEN_WASM__";
__SKYZEN_DURABLE_EXPORTS__

let initPromise;

function ensureInitialized() {
  if (!initPromise) {
    initPromise = init({ module_or_path: wasmUrl });
  }
  return initPromise;
}

// Durable Object classes are constructed by the runtime before any handler
// runs, so the wasm module must be ready at module load. Workers ESM supports
// top-level await; the awaits inside each handler are a safety net.
await ensureInitialized();

export default {
  async fetch(request, env, ctx) {
    await ensureInitialized();
    return wasmExports.fetch(request, env, ctx);
  },
__SKYZEN_EVENT_EXPORTS__};
