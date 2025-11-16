import init, { fetch as wasmFetch } from "./worker.js";
import wasmUrl from "./worker_bg.wasm";

let initPromise;

async function ensureInitialized() {
  if (!initPromise) {
    initPromise = init({ module_or_path: wasmUrl });
  }
  await initPromise;
}

export default {
  async fetch(request, env, ctx) {
    await ensureInitialized();
    return wasmFetch(request, env, ctx);
  },
};
