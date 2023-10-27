// Copyright 2018-2023 the Deno authors. All rights reserved. MIT license.

// TODO(petamoriken): enable prefer-primordials for node polyfills
// deno-lint-ignore-file prefer-primordials

import { pathFromURL } from "ext:deno_web/00_infra.js";

type ExistsCallback = (exists: boolean) => void;

/**
 * TODO: Also accept 'path' parameter as a Node polyfill Buffer type once these
 * are implemented. See https://github.com/denoland/deno/issues/3403
 * Deprecated in node api
 */
export function exists(path: string | URL, callback: ExistsCallback) {
  path = path instanceof URL ? pathFromURL(path) : path;
  Deno.lstat(path).then(() => callback(true), () => callback(false));
}

// The callback of fs.exists doesn't have standard callback signature.
// We need to provide special implementation for promisify.
// See https://github.com/nodejs/node/pull/13316
const kCustomPromisifiedSymbol = Symbol.for("nodejs.util.promisify.custom");
Object.defineProperty(exists, kCustomPromisifiedSymbol, {
  value: (path: string | URL) => {
    return new Promise((resolve) => {
      exists(path, (exists) => resolve(exists));
    });
  },
});

/**
 * TODO: Also accept 'path' parameter as a Node polyfill Buffer or URL type once these
 * are implemented. See https://github.com/denoland/deno/issues/3403
 */
export function existsSync(path: string | URL): boolean {
  path = path instanceof URL ? pathFromURL(path) : path;
  try {
    Deno.lstatSync(path);
    return true;
  } catch (_err) {
    return false;
  }
}
