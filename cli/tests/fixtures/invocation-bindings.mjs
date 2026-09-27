let next = 0;
export const pending = new Map();
export function created() { return next; }
export function createBindings(_module, onError) {
  const id = ++next;
  let panic;
  let depth = 0;
  let stranded = 0;
  class RoomObject {
    count = 0;
    fetch(action) {
      if (action === 'trap') throw new WebAssembly.RuntimeError('trap');
      if (action === 'reject') return Promise.reject(new WebAssembly.RuntimeError('trap'));
      if (action === 'error') throw new Error('not a wasm trap');
      if (action === 'panic') { panic('panic'); return id; }
      if (action === 'callback') { onError(new WebAssembly.RuntimeError('trap')); return id; }
      if (action === 'kill') { depth = 1; return id; }
      return [id, ++this.count];
    }
  }
  return {
    get depth() { return depth; },
    get stranded() { return stranded; },
    setPanicHook(callback) { panic = callback; },
    async fetch(request, _env, _ctx) {
      if (request === 'wait') await new Promise(resolve => pending.set(id, resolve));
      if (request === 'trap') throw new WebAssembly.RuntimeError('trap');
      // a surfaced Rust Err is an ordinary exception, not a wasm trap
      if (request === 'reject') throw new Error('not a wasm trap');
      // a wasm call terminated by the runtime never reaches `finally`, so its
      // depth stays elevated after the invocation dies
      if (request === 'kill') { depth = 1; return id; }
      // a scheduled microtask that never ran — dropped, or killed while
      // pending — leaves stranded positive at the next event entry
      if (request === 'strand') { stranded = 1; return id; }
      return id;
    },
    async queue() { return id; },
    async scheduled() { return id; },
    RoomObject,
  };
}
