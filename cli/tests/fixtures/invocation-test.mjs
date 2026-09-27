import assert from 'node:assert/strict';
import worker, { Room } from './worker.mjs';
import { pending, created } from './bindings.mjs';

// one application serves every invocation, including overlapping ones
const firstId = await worker.fetch('fast');
const waiting = worker.fetch('wait');
const waitingId = [...pending.keys()].at(-1);
assert.equal(await worker.fetch('fast'), firstId);
assert.equal(waitingId, firstId);

// a trap poisons the shared application: the next invocation switches
// generations, while the in-flight one on the old application is not reset
await assert.rejects(worker.fetch('trap'), WebAssembly.RuntimeError);
const nextId = await worker.fetch('fast');
assert.notEqual(nextId, firstId);
pending.get(waitingId)();
assert.equal(await waiting, waitingId);

// a simulated termination (a wasm call that never ran its finally leaves
// depth elevated) poisons the application; the next invocation switches
assert.equal(await worker.fetch('kill'), nextId);
const afterKill = await worker.fetch('fast');
assert.notEqual(afterKill, nextId);

// a scheduled microtask that never ran strands the executor; the next
// invocation switches generations
assert.equal(await worker.fetch('strand'), afterKill);
const afterStrand = await worker.fetch('fast');
assert.notEqual(afterStrand, afterKill);

// an ordinary Error thrown by a handler is not poison: the same application
// keeps serving (no new createBindings), while a RuntimeError switches
await assert.rejects(worker.fetch('reject'), /not a wasm trap/);
assert.equal(await worker.fetch('fast'), afterStrand);

// no poison means no new application across 100 invocations
const before = created();
for (let i = 0; i < 100; i++) await worker.fetch('fast');
assert.equal(created(), before);
assert.equal(await worker.queue(), await worker.scheduled());

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
const afterRejectA = a.fetch()[0];
assert.notEqual(afterRejectA, recovered[0]);
a.fetch('callback');
const afterCallback = a.fetch()[0];
assert.notEqual(afterCallback, afterRejectA);
// an ordinary Error from a method keeps the object's application
assert.throws(() => a.fetch('error'), /not a wasm trap/);
assert.equal(a.fetch()[0], afterCallback);
const beforePanic = a.fetch('panic');
const afterPanic = a.fetch()[0];
assert.notEqual(afterPanic, beforePanic);
// a killed method leaves the object's depth elevated: the next call switches
a.fetch('kill');
assert.notEqual(a.fetch()[0], afterPanic);
assert.deepEqual(b.fetch(), [bId, 3]);
console.log('shared instance with poison recovery passed');
