let next = 0;
export const pending = new Map();
export function created() { return next; }
export function createBindings(_module, onError) {
  const id = ++next;
  let panic;
  class RoomObject {
    count = 0;
    fetch(action) {
      if (action === 'trap') throw new WebAssembly.RuntimeError('trap');
      if (action === 'reject') return Promise.reject(new WebAssembly.RuntimeError('trap'));
      if (action === 'panic') { panic('panic'); return id; }
      if (action === 'callback') { onError(new WebAssembly.RuntimeError('trap')); return id; }
      return [id, ++this.count];
    }
  }
  return {
    setPanicHook(callback) { panic = callback; },
    async fetch(request, _env, ctx) {
      if (request === 'wait') await new Promise(resolve => pending.set(id, resolve));
      if (request === 'trap') throw new WebAssembly.RuntimeError('trap');
      if (request === 'hold') ctx.waitUntil(new Promise(resolve => pending.set('hold', resolve)));
      if (request === 'stream') {
        return new Response(new ReadableStream({
          start(controller) {
            controller.enqueue(new TextEncoder().encode(String(id)));
            pending.set('stream', controller);
          },
        }));
      }
      return id;
    },
    async queue() { return id; },
    async scheduled() { return id; },
    RoomObject,
  };
}
