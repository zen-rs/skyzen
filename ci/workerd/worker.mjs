import init, { fetch as wasmFetch } from "./worker.js";
import wasmUrl from "./worker_bg.wasm";
export { VisitsObject } from "./worker.js";

let initPromise;

function ensureInitialized() {
  if (!initPromise) {
    initPromise = init({ module_or_path: wasmUrl });
  }
  return initPromise;
}

// Durable Object classes are constructed by the runtime before any handler
// runs, so the wasm module must be ready at module load. Workers ESM supports
// top-level await; the await inside the handler is a safety net.
await ensureInitialized();

export default {
  async fetch(request, env, ctx) {
    await ensureInitialized();
    return wasmFetch(request, env, ctx);
  },
};
