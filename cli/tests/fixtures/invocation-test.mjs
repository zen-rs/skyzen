import assert from 'node:assert/strict';
import worker, { Room } from './worker.mjs';
import { pending, created } from './bindings.mjs';

// Node lacks workerd's IdentityTransformStream; an identity TransformStream
// is the same byte pipe for these tests. Referenced only at request time,
// so installing it after the imports is safe.
globalThis.IdentityTransformStream ??= class IdentityTransformStream extends TransformStream {};

const waiting = worker.fetch('wait');
const [waitingId, release] = pending.entries().next().value;
const fastId = await worker.fetch('fast');
assert.notEqual(fastId, waitingId);
await assert.rejects(worker.fetch('trap'), WebAssembly.RuntimeError);
// the trap's application is dropped, so the next fetch runs on a fresh one
const nextId = await worker.fetch('fast');
assert.notEqual(nextId, fastId);
release();
assert.equal(await waiting, waitingId);
// sequential invocations reuse the one warm spare
assert.equal(await worker.queue(), await worker.scheduled());

const tick = () => new Promise(r => setTimeout(r, 0));

// sequential reuse: no new application is created
const seqId = await worker.fetch('fast');
assert.equal(await worker.fetch('fast'), seqId);
const beforeOverlap = created();
await worker.fetch('fast');
assert.equal(created(), beforeOverlap);

// two overlapping invocations never share an application
const overlapWait = worker.fetch('wait');
const overlapFast = await worker.fetch('fast');
assert.notEqual(overlapFast, seqId);
assert.equal(created(), beforeOverlap + 1);
const overlapWaitId = [...pending.keys()].at(-1);
pending.get(overlapWaitId)();
assert.equal(await overlapWait, overlapWaitId);
// the slot already holds overlapFast's spare, so the wait application's
// release drops it; the spare keeps serving
assert.equal(await worker.fetch('fast'), overlapFast);

// an invocation whose promise never settles keeps its application out of the
// slot while later invocations succeed on other instances
const stuck = worker.fetch('wait');
const stuckId = [...pending.keys()].at(-1);
const r1 = await worker.fetch('fast');
const r2 = await worker.fetch('fast');
assert.equal(r1, r2);
assert.notEqual(r1, stuckId);
pending.get(stuckId)();
assert.equal(await stuck, stuckId);
await tick();
assert.equal(await worker.fetch('fast'), r1);

// an unsettled ctx.waitUntil promise holds the lease
const heldId = await worker.fetch('hold', {}, { waitUntil() {} });
const duringHold = await worker.fetch('fast');
assert.notEqual(duringHold, heldId);
pending.get('hold')();
await tick();
assert.equal(await worker.fetch('fast'), duringHold);

// a streaming body holds the lease until the stream is cancelled
const res = await worker.fetch('stream');
const reader = res.body.getReader();
const streamId = Number(new TextDecoder().decode((await reader.read()).value));
const duringStream = await worker.fetch('fast');
assert.notEqual(duringStream, streamId);
await reader.cancel();
await tick();
assert.equal(await worker.fetch('fast'), duringStream);

const a = new Room();
const b = new Room();
const savedMethod = a.fetch;
const aId = a.fetch()[0];
const bId = b.fetch()[0];
assert.notEqual(aId, bId);
assert.deepEqual(a.fetch(), [aId, 2]);
assert.throws(() => a.fetch('trap'), WebAssembly.RuntimeError);
const recovered = savedMethod();
assert.notEqual(recovered[0], aId);
assert.equal(recovered[1], 1);
assert.deepEqual(b.fetch(), [bId, 2]);
await assert.rejects(a.fetch('reject'), WebAssembly.RuntimeError);
const afterReject = a.fetch()[0];
assert.notEqual(afterReject, recovered[0]);
a.fetch('callback');
assert.notEqual(a.fetch()[0], afterReject);
const beforePanic = a.fetch('panic');
assert.notEqual(a.fetch()[0], beforePanic);
assert.deepEqual(b.fetch(), [bId, 3]);
console.log('invocation isolation and object-local recovery passed');
