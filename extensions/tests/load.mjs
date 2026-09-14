// Copyright (C) 2026 Javad Rajabzadeh
// SPDX-License-Identifier: GPL-3.0-or-later
//
// Load a browser's background script into a vm context the way that browser
// does: the shared core first, then the browser's own capture path, all in
// one global scope.
import { readFileSync } from "node:fs";
import vm from "node:vm";

export const CORE = "extensions/chrome/core.js";

const run = (ctx, file) => vm.runInContext(readFileSync(file, "utf8"), ctx, { filename: file });

export function loadBackground(ctx, browser = "chrome") {
  if (browser === "chrome") {
    // The service worker pulls core.js in itself.
    ctx.importScripts = (...files) => files.forEach((f) => run(ctx, `extensions/chrome/${f}`));
    run(ctx, "extensions/chrome/background.js");
  } else {
    // The manifest lists core.js before background.js. Firefox's copy of
    // core.js is a build artefact, so read the original.
    run(ctx, CORE);
    run(ctx, `extensions/${browser}/background.js`);
  }
}
