// Copyright 2018-2023 the Deno authors. All rights reserved. MIT license.
import { assert, assertEquals } from "./test_util.ts";

Deno.test(async function metrics() {
  // Write to stdout to ensure a "data" message gets sent instead of just
  // control messages.
  const dataMsg = new Uint8Array([13, 13, 13]); // "\r\r\r",
  await Deno.stdout.write(dataMsg);

  // WARNING: bytesReceived & bytesSentControl are now always zero
  // following https://github.com/denoland/deno/pull/9843

  const m1 = Deno.metrics();
  assert(m1.opsDispatched > 0);
  assert(m1.opsCompleted > 0);
  assert(m1.bytesSentControl === 0);
  assert(m1.bytesSentData === 0);
  assert(m1.bytesReceived === 0);
  const m1OpWrite = m1.ops["op_write"];
  assert(m1OpWrite.opsDispatchedAsync > 0);
  assert(m1OpWrite.opsCompletedAsync > 0);
  assert(m1OpWrite.bytesSentControl === 0);
  assert(m1OpWrite.bytesSentData >= 0);
  assert(m1OpWrite.bytesReceived === 0);

  await Deno.stdout.write(dataMsg);

  const m2 = Deno.metrics();
  assert(m2.opsDispatchedAsync > m1.opsDispatchedAsync);
  assert(m2.opsCompletedAsync > m1.opsCompletedAsync);
  assert(m2.bytesSentControl === m1.bytesSentControl);
  assert(m2.bytesSentData === 0);
  assert(m2.bytesReceived === m1.bytesReceived);
  const m2OpWrite = m2.ops["op_write"];
  assert(m2OpWrite.opsDispatchedAsync > m1OpWrite.opsDispatchedAsync);
  assert(m2OpWrite.opsCompletedAsync > m1OpWrite.opsCompletedAsync);
  assert(m2OpWrite.bytesSentControl === m1OpWrite.bytesSentControl);
  assert(m2OpWrite.bytesSentData === 0);
  assert(m2OpWrite.bytesReceived === m1OpWrite.bytesReceived);
});

Deno.test(
  { permissions: { write: true } },
  function metricsUpdatedIfNoResponseSync() {
    const filename = Deno.makeTempDirSync() + "/test.txt";

    const data = new Uint8Array([41, 42, 43]);
    Deno.writeFileSync(filename, data, { mode: 0o666 });

    const metrics = Deno.metrics();
    assert(metrics.opsDispatched === metrics.opsCompleted);
    assert(metrics.opsDispatchedSync === metrics.opsCompletedSync);
  },
);

Deno.test(
  { permissions: { write: true } },
  async function metricsUpdatedIfNoResponseAsync() {
    const filename = Deno.makeTempDirSync() + "/test.txt";

    const data = new Uint8Array([41, 42, 43]);
    await Deno.writeFile(filename, data, { mode: 0o666 });

    const metrics = Deno.metrics();
    assert(metrics.opsDispatched === metrics.opsCompleted);
    assert(metrics.opsDispatchedSync === metrics.opsCompletedSync);
    assert(metrics.opsDispatchedAsync === metrics.opsCompletedAsync);
  },
);

// Test that ops from extensions have metrics (via OpMiddleware)
Deno.test.ignore(function metricsForOpCrates() {
  const _ = new URL("https://deno.land");

  const m1 = Deno.metrics().ops["op_url_parse"];
  assert(m1.opsDispatched > 0);
  assert(m1.opsCompleted > 0);
});

// Test that op_names == Objects.keys(Deno[Deno.internal].core.ops)
// since building the per-op metrics depends on op_names being complete
Deno.test(function opNamesMatch() {
  // @ts-ignore: Deno[Deno.internal].core allowed
  const ops = Object.keys(Deno[Deno.internal].core.ops);
  // @ts-ignore: Deno[Deno.internal].core allowed
  ops.concat(Object.keys(Deno[Deno.internal].core.asyncOps));

  assertEquals(
    // @ts-ignore: Deno[Deno.internal].core allowed
    Deno[Deno.internal].core.opNames().sort(),
    ops.sort().filter((name) => name !== "asyncOpsInfo"),
  );
});
