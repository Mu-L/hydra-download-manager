// Copyright (C) 2026 Javad Rajabzadeh
// SPDX-License-Identifier: GPL-3.0-or-later
//
// Which clicks the content script treats as "hold Alt and let the browser
// have this one". Gecko is the only browser the extension saves for, so the
// gesture test is where a wrong answer costs a download.
// Run from the repository root:  node extensions/tests/altclick.test.mjs
import { readFileSync } from "node:fs";

const src = readFileSync("extensions/chrome/content.js", "utf8");

// Lift the pure decision out of the content script, with `navigator` handed
// in so each browser's user agent can be tried against the real source.
function load(userAgent) {
  const constAt = src.indexOf("const ALT_SAVE_IS_OURS");
  const fnAt = src.indexOf("function altSaveUrl(");
  if (constAt < 0 || fnAt < 0) throw new Error("missing the Alt-save decision");
  const code =
    src.slice(constAt, src.indexOf("\n", constAt) + 1) +
    src.slice(fnAt, src.indexOf("\n}\n", fnAt) + 3);
  return new Function("navigator", code + "return altSaveUrl;")({ userAgent });
}

const FIREFOX =
  "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:155.0) Gecko/20100101 Firefox/155.0";
const CHROME =
  "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) " +
  "Chrome/120.0.0.0 Safari/537.36";
const SAFARI =
  "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) " +
  "Version/17.0 Safari/605.1.15";

let fails = 0;
const eq = (label, got, want) => {
  if (got === want) console.log(`ok   ${label}`);
  else {
    fails++;
    console.log(`FAIL ${label}\n  got  ${JSON.stringify(got)}\n  want ${JSON.stringify(want)}`);
  }
};

// A click, shaped the way the listener reads one. `href` null stands for a
// click that landed on no link at all.
const click = ({ href = "https://cdn.example/pack.zip", ...rest } = {}) => ({
  altKey: true,
  button: 0,
  defaultPrevented: false,
  target: { closest: () => (href === null ? null : { href }) },
  ...rest,
});

const onFirefox = load(FIREFOX);

eq("firefox: an Alt+click on a link is ours to save", onFirefox(click()), "https://cdn.example/pack.zip");
eq("chrome saves its own links", load(CHROME)(click()), null);
eq("safari saves its own links", load(SAFARI)(click()), null);

eq("a plain click is left alone", onFirefox(click({ altKey: false })), null);
eq("a middle-click is a new tab, not a save", onFirefox(click({ button: 1 })), null);
eq("a click on no link is left alone", onFirefox(click({ href: null })), null);

// The page may own Alt+click for something of its own; by the bubble phase
// its preventDefault() has already been recorded.
eq("a page that claimed the click keeps it", onFirefox(click({ defaultPrevented: true })), null);

// Only schemes the browser's download manager can fetch: the URL crosses into
// downloads.download(), so a javascript: or file: href must not.
eq("a javascript: link is not a download", onFirefox(click({ href: "javascript:alert(1)" })), null);
eq("a file: link is not a download", onFirefox(click({ href: "file:///etc/passwd" })), null);
eq("http is saved as readily as https", onFirefox(click({ href: "http://cdn.example/a.zip" })), "http://cdn.example/a.zip");

console.log(fails ? `\n${fails} failed` : "\nall passed");
process.exit(fails ? 1 : 0);
