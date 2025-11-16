import init, { fetch as wasmFetch } from "./worker.js";

let initPromise;

async function ensureInitialized() {
  if (!initPromise) {
    initPromise = init();
  }
  await initPromise;
}

export default {
  async fetch(request, env, ctx) {
    await ensureInitialized();
    return wasmFetch(request, env, ctx);
  },
};
