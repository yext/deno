// Copyright 2018-2023 the Deno authors. All rights reserved. MIT license.
import { assertEquals } from "../../../test_util/std/assert/mod.ts";
import { deferred } from "../../../test_util/std/async/deferred.ts";

Deno.test("BroadcastChannel worker", async () => {
  const intercom = new BroadcastChannel("intercom");
  let count = 0;

  const url = import.meta.resolve(
    "../testdata/workers/broadcast_channel.ts",
  );
  const worker = new Worker(url, { type: "module", name: "worker" });
  worker.onmessage = () => intercom.postMessage(++count);

  const promise = deferred();

  intercom.onmessage = function (e) {
    assertEquals(count, e.data);
    if (count < 42) {
      intercom.postMessage(++count);
    } else {
      worker.terminate();
      intercom.close();
      promise.resolve();
    }
  };

  await promise;
});

Deno.test("BroadcastChannel immediate close after post", () => {
  const bc = new BroadcastChannel("internal_notification");
  bc.postMessage("New listening connected!");
  bc.close();
});
