"use strict";

((window) => {
  const Console = window.__bootstrap.console.Console;
  const { ObjectDefineProperty, SymbolFor } = window.__bootstrap.primordials;

  const yextInternal = SymbolFor("Yext.Internal");
  ObjectDefineProperty(globalThis, yextInternal, {
    value: { Console },
    configurable: true,
  });
})(this);
