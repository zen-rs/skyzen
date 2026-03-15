import init, { fetch as wasmFetch } from "./__SKYZEN_BINDINGS_JS__";
import wasmUrl from "./__SKYZEN_WASM__";

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
