// Copyright (C) 2026 Javad Rajabzadeh
// SPDX-License-Identifier: GPL-3.0-or-later
//
// Automatic capture, the context menu and the selection pill, driven through
// the real service worker with a mocked browser and a fake Hydra.
// Run from the repository root:  node extensions/tests/capture.test.mjs
import { readFileSync } from "node:fs";
import vm from "node:vm";

const SRC = readFileSync("extensions/chrome/background.js", "utf8");
const UA =
  "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

let fails = 0;
const check = (label, cond, extra = "") => {
  if (cond) console.log(`ok   ${label}`);
  else { fails++; console.log(`FAIL ${label} ${extra}`); }
};
const tick = (n = 3) => new Promise((r) => { let i = 0; const f = () => (++i >= n ? r() : setImmediate(f)); setImmediate(f); });

function build({ hydraReply = { ok: true }, store = {}, gecko = false, tab = null } = {}) {
  const sent = [];          // messages that reached "Hydra"
  const calls = [];         // downloads API calls
  const listeners = {};
  const ev = () => { const l = []; return { addListener: (f) => l.push(f), fire: (...a) => l.map((f) => f(...a)), l }; };

  const storage = () => ({
    async get(def) {
      if (typeof def === "string") return { [def]: store[def] };
      const out = {};
      for (const [k, v] of Object.entries(def)) out[k] = k in store ? store[k] : v;
      return out;
    },
    async set(patch) { Object.assign(store, patch); },
    async remove(keys) { for (const k of [].concat(keys)) delete store[k]; },
  });

  const onCreated = ev(), onDetermining = ev(), onMenu = ev();
  const chrome = {
    runtime: {
      id: "hydra-ext",
      lastError: null,
      onInstalled: ev(), onStartup: ev(),
      onMessage: (listeners.msg = ev()),
      getURL: (p) => `chrome-extension://x/${p}`,
      sendNativeMessage: (_h, _m, cb) => cb(undefined),
    },
    storage: { local: storage(), session: storage() },
    action: {
      setBadgeBackgroundColor: async () => {}, setBadgeText: async () => {}, setTitle: async () => {},
    },
    downloads: {
      onCreated,
      // Gecko has no onDeterminingFilename at all — the capture path keys
      // off its absence, so a mock that always offers it can only ever test
      // the Chromium half.
      ...(gecko ? {} : { onDeterminingFilename: onDetermining }),
      download: async (opts) => calls.push(["download", opts.url]),
      pause: async (id) => calls.push(["pause", id]),
      resume: async (id) => calls.push(["resume", id]),
      cancel: async (id) => calls.push(["cancel", id]),
      erase: async (q) => calls.push(["erase", q.id]),
    },
    contextMenus: { removeAll: async () => {}, create: () => {}, onClicked: onMenu },
    tabs: {
      query: async () => [{ id: 7, url: "https://page.example/watch" }],
      get: async () => tab ?? { id: 7, url: "https://page.example/watch", title: "Sunset Timelapse 4K" },
      remove: async (id) => calls.push(["tabRemove", id]),
      sendMessage: async () => ({ urls: ["https://a.example/1.zip", "https://b.example/2.zip"] }),
      create: () => {}, onRemoved: ev(), onUpdated: ev(),
    },
    cookies: { getAll: async () => [{ name: "sid", value: "abc" }] },
    webRequest: {
      onResponseStarted: {
        addListener: (fn, filter) => {
          listeners.sniff = fn;
          listeners.sniffFilter = filter;
        },
      },
      onHeadersReceived: {
        addListener: (fn, filter, spec) => {
          listeners.headers = fn;
          listeners.headersFilter = filter;
          listeners.headersSpec = spec;
        },
      },
      onBeforeRedirect: {
        addListener: (fn) => { listeners.redirect = fn; },
      },
    },
  };

  class FakeWS {
    constructor() { setImmediate(() => this.onopen && this.onopen()); }
    send(raw) {
      const msg = JSON.parse(raw);
      sent.push(msg);
      const reply = msg.type === "ping" || msg.type === "config"
        ? { ok: true } : hydraReply;
      setImmediate(() => this.onmessage && this.onmessage({
        data: JSON.stringify({ ...reply, id: msg.id, capture: true, auto_types: "ZIP MP4 EXE", dont_start_sites: "*.blocked.example" }),
      }));
    }
    close() {}
  }

  const ctx = vm.createContext({
    chrome, navigator: { userAgent: UA }, WebSocket: FakeWS, console,
    setTimeout, clearTimeout, setInterval, clearInterval, setImmediate, fetch: async () => { throw new Error("no net"); },
    // A fresh vm context has the ECMAScript built-ins only; URL and friends
    // are web globals the service worker relies on, so hand them in.
    AbortController, URL, URLSearchParams, TextDecoder,
  });
  vm.runInContext(SRC, ctx);
  const send = (msg) => new Promise((res) => { listeners.msg.l[0](msg, {}, res); });
  // One response, shaped as chrome.webRequest delivers it — and gated by the
  // registered `types` filter, exactly as the browser gates it. Calling the
  // listener directly would let a test pass against a filter that never
  // delivers the response in the first place.
  const respond = ({ url, mime, size = 4e6, type = "media", tabId = 7 }) =>
    (listeners.sniffFilter?.types || []).includes(type) &&
    listeners.sniff?.({
      tabId,
      url,
      type,
      responseHeaders: [
        { name: "Content-Type", value: mime },
        { name: "Content-Length", value: String(size) },
      ],
    });
  // One response as the browser hands it to a BLOCKING listener, gated by the
  // registered `types` filter the way the browser gates it: a test that called
  // the listener directly would pass against a filter the response never
  // reaches. Returns whatever the listener returns — the blocking verdict.
  const headResponse = ({ url, headers = {}, type = "main_frame", tabId = 7, requestId = "r1" }) =>
    (listeners.headersFilter?.types || []).includes(type)
      ? listeners.headers?.({
          url, type, tabId, requestId,
          responseHeaders: Object.entries(headers).map(([name, value]) => ({ name, value })),
        })
      : undefined;
  const redirected = ({ url, redirectUrl, requestId = "r1" }) =>
    listeners.redirect?.({ url, redirectUrl, requestId, type: "main_frame", tabId: 7 });

  return {
    sent, calls, onCreated, onDetermining, onMenu, send, store, respond,
    headResponse, redirected,
    sniffFilter: () => listeners.sniffFilter,
    headersSpec: () => listeners.headersSpec,
  };
}

// ---------------------------------------------------- 1. automatic capture
{
  const h = build();
  await tick(4);
  const item = { id: 1, url: "https://cdn.example/pack.zip", filename: "pack.zip", mime: "application/zip", totalBytes: 5e6, referrer: "https://page.example/" };
  h.onCreated.fire(item);
  await tick(4);
  check("capture: browser download is paused on creation", h.calls.some(([c, id]) => c === "pause" && id === 1), JSON.stringify(h.calls));

  h.onDetermining.fire(item, () => h.calls.push(["suggest"]));
  await tick(8);
  const dl = h.sent.find((m) => m.type === "download");
  check("capture: onDeterminingFilename calls suggest()", h.calls.some(([c]) => c === "suggest"));
  check("capture: matching type is handed to Hydra", !!dl, JSON.stringify(h.sent));
  check("capture: the URL is forwarded", dl?.url === "https://cdn.example/pack.zip");
  check("capture: cookies ride along", dl?.cookies === "sid=abc");
  check("capture: referer + size + mime ride along", dl?.referer === "https://page.example/" && dl?.size === 5e6 && dl?.mime === "application/zip");
  check("capture: the request names its browser", dl?.browser === "Google Chrome", JSON.stringify(dl));
  check("capture: browser's copy is cancelled and erased", h.calls.some(([c]) => c === "cancel") && h.calls.some(([c]) => c === "erase"));
  check("capture: it is NOT resumed once Hydra took it", !h.calls.some(([c]) => c === "resume"));
}

// ------------------------------------- 2. a type outside the capture list
{
  const h = build();
  await tick(4);
  const item = { id: 2, url: "https://cdn.example/page.html", filename: "page.html", mime: "text/html" };
  h.onCreated.fire(item); await tick(3);
  h.onDetermining.fire(item, () => {}); await tick(6);
  check("passthrough: unlisted type is not sent to Hydra", !h.sent.some((m) => m.type === "download"));
  check("passthrough: the browser download resumes", h.calls.some(([c]) => c === "resume"));
}

// ------------------------------------------------------ 3. Alt-click bypass
{
  const h = build({ store: { altTs: Date.now() } });
  await tick(4);
  const item = { id: 3, url: "https://cdn.example/pack.zip", filename: "pack.zip", mime: "application/zip" };
  h.onDetermining.fire(item, () => {}); await tick(6);
  check("alt bypass: nothing is sent to Hydra", !h.sent.some((m) => m.type === "download"));
  check("alt bypass: the browser keeps the download", h.calls.some(([c]) => c === "resume"));
}

// ------------------------------- 3b. the Gecko stand-in for an Alt+click save
//
// `browser.altClickSave` has defaulted to false since Firefox 13, so standing
// aside on Gecko left the user with no download anywhere. The extension saves
// the link itself there; `captureEligible` skips its own downloads, so the
// file cannot bounce straight back into Hydra.
{
  const h = build({ gecko: true });
  await tick(4);
  await h.send({ type: "alt-download", url: "https://cdn.example/pack.zip" });
  check("alt stand-in: the browser is asked to download the link",
    h.calls.some(([c, u]) => c === "download" && u === "https://cdn.example/pack.zip"),
    JSON.stringify(h.calls));
  check("alt stand-in: nothing is sent to Hydra", !h.sent.some((m) => m.type === "download"));

  const refused = await h.send({ type: "alt-download", url: "javascript:alert(1)" });
  check("alt stand-in: a non-http link is refused", refused?.ok === false);
  check("alt stand-in: and never reaches the downloads API",
    h.calls.filter(([c]) => c === "download").length === 1, JSON.stringify(h.calls));
}

// ------------------------------------------------------- 4. excluded site
{
  const h = build();
  await tick(12); // let the config round-trip land the site list first
  const item = { id: 4, url: "https://dl.blocked.example/pack.zip", filename: "pack.zip", mime: "application/zip" };
  h.onDetermining.fire(item, () => {}); await tick(8);
  check("skip list: an excluded site is left to the browser", !h.sent.some((m) => m.type === "download") && h.calls.some(([c]) => c === "resume"), JSON.stringify({sent:h.sent.map(m=>m.type), calls:h.calls}));
}

// ------------------------------------------------- 5. Hydra says no / down
{
  const h = build({ hydraReply: { ok: false, error: "nope" } });
  await tick(4);
  const item = { id: 5, url: "https://cdn.example/pack.zip", filename: "pack.zip", mime: "application/zip" };
  h.onDetermining.fire(item, () => {}); await tick(8);
  check("fallback: a refused hand-off resumes the browser download", h.calls.some(([c]) => c === "resume"));
  check("fallback: the paused copy is not cancelled", !h.calls.some(([c]) => c === "cancel"));
}

// ------------------------------------------------ 5b. the Gecko capture path
//
// `downloads.pause()` on Gecko is `download.cancel()`, and `resume()` needs
// partial data a download at byte 0 has none of. So nothing there may be
// parked speculatively: a download the extension touches and hands back is
// stuck "Canceled" for good.
{
  const h = build({ gecko: true });
  await tick(12);
  const item = { id: 51, url: "https://cdn.example/photo.png", filename: "photo.png", mime: "image/png" };
  h.onCreated.fire(item); await tick(8);
  check("gecko: an unlisted type is never touched",
    h.calls.length === 0 && !h.sent.some((m) => m.type === "download"), JSON.stringify(h.calls));
}
{
  const h = build({ gecko: true });
  await tick(12);
  const item = { id: 52, url: "https://cdn.example/pack.zip", filename: "pack.zip", mime: "application/zip", totalBytes: 5e6 };
  h.onCreated.fire(item); await tick(10);
  check("gecko: a matching type reaches Hydra without being paused",
    h.sent.some((m) => m.type === "download") && !h.calls.some(([c]) => c === "pause"), JSON.stringify(h.calls));
  check("gecko: the browser's copy is cancelled once Hydra has it",
    h.calls.some(([c]) => c === "cancel") && h.calls.some(([c]) => c === "erase"));
}
{
  const h = build({ gecko: true, hydraReply: { ok: false, error: "nope" } });
  await tick(12);
  const item = { id: 53, url: "https://cdn.example/pack.zip", filename: "pack.zip", mime: "application/zip" };
  h.onCreated.fire(item); await tick(10);
  check("gecko: a refused hand-off leaves the browser download running",
    !h.calls.some(([c]) => c === "cancel") && !h.calls.some(([c]) => c === "resume"), JSON.stringify(h.calls));
}

// ------------------------------- 5c. the Gecko capture point: before a download
//
// Cancelling a download on Gecko leaves a "Canceled" row in the browser's own
// list for every file Hydra takes, and nothing can erase the browser's memory
// of it. So the decision is made one event earlier, off the response headers:
// a response cancelled there never becomes a download at all.
{
  const h = build({ gecko: true });
  await tick(12);
  check("gecko headers: the listener is registered as blocking",
    (h.headersSpec() || []).includes("blocking"), JSON.stringify(h.headersSpec()));

  const verdict = await h.headResponse({
    url: "https://cdn.example/pack.zip",
    headers: {
      "Content-Type": "application/zip",
      "Content-Length": "5000000",
      "Content-Disposition": 'attachment; filename="pack.zip"',
    },
  });
  check("gecko headers: a matching response is cancelled before it downloads",
    verdict?.cancel === true, JSON.stringify(verdict));
  const dl = h.sent.find((m) => m.type === "download");
  check("gecko headers: it reaches Hydra with name, size and mime",
    dl?.url === "https://cdn.example/pack.zip" && dl?.filename === "pack.zip" &&
      dl?.size === 5e6 && dl?.mime === "application/zip",
    JSON.stringify(dl));
  check("gecko headers: no download record is created, so none is cancelled",
    h.calls.length === 0, JSON.stringify(h.calls));
}

// What the browser SHOWS is not a download, and cancelling it would take the
// page away from the user.
{
  const h = build({ gecko: true });
  await tick(12);
  const page = await h.headResponse({
    url: "https://page.example/index.html",
    headers: { "Content-Type": "text/html; charset=utf-8" },
  });
  const pdf = await h.headResponse({
    url: "https://cdn.example/manual.pdf",
    headers: { "Content-Type": "application/pdf" },
  });
  check("gecko headers: a page the browser renders is left alone", page === undefined);
  check("gecko headers: a PDF the built-in viewer opens is left to the viewer", pdf === undefined);
  check("gecko headers: neither is offered to Hydra", !h.sent.some((m) => m.type === "download"));
}

// A response Hydra refuses becomes a download — and that download must not
// ask the same question again: between the two asks the native host may have
// LAUNCHED the app, and the second answer is the cancelled row we avoided.
{
  const h = build({ gecko: true, hydraReply: { ok: false, error: "nope" } });
  await tick(12);
  const verdict = await h.headResponse({
    url: "https://cdn.example/pack.zip",
    headers: { "Content-Type": "application/zip" },
  });
  check("gecko headers: a refused response is not cancelled", !verdict?.cancel, JSON.stringify(verdict));
  const asked = h.sent.filter((m) => m.type === "download").length;
  h.onCreated.fire({ id: 61, url: "https://cdn.example/pack.zip", filename: "pack.zip", mime: "application/zip" });
  await tick(10);
  check("gecko headers: the download it becomes does not ask a second time",
    h.sent.filter((m) => m.type === "download").length === asked,
    JSON.stringify(h.sent.map((m) => m.type)));
  check("gecko headers: and is never cancelled",
    !h.calls.some(([c]) => c === "cancel"), JSON.stringify(h.calls));
}

// The name lives in Content-Disposition when the URL carries no extension —
// which is most "download.php?id=" links.
{
  const h = build({ gecko: true });
  await tick(12);
  const verdict = await h.headResponse({
    url: "https://cdn.example/download?id=884",
    headers: {
      "Content-Type": "application/octet-stream",
      "Content-Disposition": "attachment; filename*=UTF-8''r%C3%A9sum%C3%A9.zip",
    },
  });
  check("gecko headers: an RFC 5987 filename supplies the extension the URL lacks",
    verdict?.cancel === true &&
      h.sent.find((m) => m.type === "download")?.filename === "résumé.zip",
    JSON.stringify(h.sent));
}

// The Alt bypass saves the link through `downloads.download()` on Gecko. The
// header stage cannot see who asked, so cancelling here would defeat the very
// bypass the download exists to honour.
{
  const h = build({ gecko: true });
  await tick(12);
  await h.send({ type: "alt-download", url: "https://cdn.example/pack.zip" });
  const verdict = await h.headResponse({
    url: "https://cdn.example/pack.zip",
    headers: { "Content-Type": "application/zip" },
  });
  check("gecko headers: the Alt bypass's own download survives",
    !verdict?.cancel && !h.sent.some((m) => m.type === "download"), JSON.stringify(verdict));
}

// The net runs the other way round too. "Save Link As" and a retry from the
// downloads panel create the download BEFORE the response comes back, so the
// header stage would otherwise offer the same transfer a second time and Hydra
// would queue it twice.
{
  const h = build({ gecko: true });
  await tick(12);
  h.onCreated.fire({ id: 71, url: "https://cdn.example/pack.zip", filename: "pack.zip", mime: "application/zip" });
  await tick(10);
  check("gecko: a download the browser started first still reaches Hydra",
    h.sent.filter((m) => m.type === "download").length === 1, JSON.stringify(h.sent.map((m) => m.type)));
  const verdict = await h.headResponse({
    url: "https://cdn.example/pack.zip",
    headers: { "Content-Type": "application/zip" },
  });
  check("gecko: and its response is not offered a second time",
    verdict === undefined && h.sent.filter((m) => m.type === "download").length === 1,
    JSON.stringify(h.sent.map((m) => m.type)));
}

// A redirect to a signature good for ten seconds: our copy of it is dead by
// Hydra's first retry, and only the link that minted it can be replayed.
{
  const h = build({ gecko: true });
  await tick(12);
  const stamp = new Date().toISOString().replace(/[-:]/g, "").replace(/\.\d+/, "");
  const signed = `https://store.example/blob?X-Amz-Date=${stamp}&X-Amz-Expires=10&X-Amz-Signature=dead`;
  h.redirected({ url: "https://files.example/pack.zip", redirectUrl: signed, requestId: "r9" });
  const verdict = await h.headResponse({
    url: signed,
    requestId: "r9",
    headers: { "Content-Type": "application/zip" },
  });
  check("gecko headers: a short-lived signature is taken, not handed over",
    verdict?.cancel === true &&
      h.sent.find((m) => m.type === "download")?.url === "https://files.example/pack.zip",
    JSON.stringify(h.sent.find((m) => m.type === "download")));
}

// A download link with target="_blank" opens a tab the browser would have
// closed itself once the response turned into a download.
{
  const h = build({ gecko: true, tab: { id: 9, url: "about:blank", openerTabId: 7 } });
  await tick(12);
  await h.headResponse({
    url: "https://cdn.example/pack.zip",
    tabId: 9,
    headers: { "Content-Type": "application/zip" },
  });
  await tick(6);
  check("gecko headers: the blank tab the link opened is closed after all",
    h.calls.some(([c, id]) => c === "tabRemove" && id === 9), JSON.stringify(h.calls));
}

// ------------------------------------------- 6. right-click a link / media
{
  const h = build();
  await tick(4);
  await h.onMenu.fire({ menuItemId: "hydra-link", linkUrl: "https://files.example/a.mp4" }, { url: "https://page.example/" })[0];
  await tick(6);
  const dl = h.sent.find((m) => m.type === "download");
  check("context menu: 'Download with Hydra' sends the link", dl?.url === "https://files.example/a.mp4", JSON.stringify(h.sent));
  check("context menu: page URL travels as referer/tab_url", dl?.referer === "https://page.example/" && dl?.tab_url === "https://page.example/");

  const h2 = build(); await tick(4);
  await h2.onMenu.fire({ menuItemId: "hydra-media", srcUrl: "https://files.example/v.webm" }, { url: "https://page.example/" })[0];
  await tick(6);
  check("context menu: image/video/audio uses srcUrl", h2.sent.find((m) => m.type === "download")?.url === "https://files.example/v.webm");
}

// --------------------------------------------------- 7. the selection pill
{
  const h = build(); await tick(4);
  const one = await h.send({ type: "selection-links", urls: ["https://files.example/only.zip"], referer: "https://page.example/" });
  await tick(2);
  check("selection: a single highlighted link downloads directly", h.sent.some((m) => m.type === "download" && m.url === "https://files.example/only.zip"), JSON.stringify(one));

  const h2 = build(); await tick(4);
  await h2.send({ type: "selection-links", urls: ["https://a.example/1.zip", "https://b.example/2.zip"], referer: "https://page.example/" });
  await tick(2);
  const batch = h2.sent.find((m) => m.type === "links");
  check("selection: several links open the batch box", batch?.urls?.length === 2, JSON.stringify(h2.sent));

  const h3 = build(); await tick(4);
  await h3.send({ type: "download-all-links" });
  await tick(2);
  check("'Download all links' collects the page and batches them", h3.sent.find((m) => m.type === "links")?.urls?.length === 2);
}

// ------------------------------------------------ 8. welcome page's status
{
  const h = build(); await tick(4);
  const st = await h.send({ type: "status" });
  check("welcome: status reports a live app", st?.app === true && st?.host === true, JSON.stringify(st));
}

// ------------------------------------------- N. audio played in the browser
{
  const h = build();
  await tick(4);

  // An <audio> element loads as `media`, but a LINK to an .mp3 — the browser
  // opening its own player, which is what "played in the browser" usually
  // means — is a top-level navigation. Watching only `media` meant those
  // responses never reached the sniffer at all.
  const types = h.sniffFilter()?.types || [];
  check(
    "sniffer: navigations are watched, not just media elements",
    types.includes("main_frame") && types.includes("sub_frame"),
    JSON.stringify(types)
  );

  for (const [mime, url] of [
    ["audio/mpeg", "https://cdn.example/song.mp3"],
    ["audio/wav", "https://cdn.example/take.wav"],
    ["audio/ogg", "https://cdn.example/clip.ogg"],
    ["audio/flac", "https://cdn.example/master.flac"],
    ["audio/x-m4a", "https://cdn.example/voice.m4a"],
    ["audio/opus", "https://cdn.example/note.opus"],
  ]) {
    const t = build();
    await tick(2);
    t.respond({ url, mime, type: "main_frame" });
    await tick(6);
    const list = t.store.media_7 || [];
    check(
      `sniffer: ${mime} played in the tab is caught`,
      list.some((m) => m.url === url),
      JSON.stringify(list)
    );
  }

  // A file server that does not know the type says octet-stream; that is not
  // a reason to lose a song.
  {
    const t = build();
    await tick(2);
    t.respond({ url: "https://files.example/album/track.mp3", mime: "application/octet-stream", type: "main_frame" });
    await tick(6);
    check(
      "sniffer: octet-stream with an audio extension is caught",
      (t.store.media_7 || []).some((m) => m.url.endsWith("track.mp3")),
      JSON.stringify(t.store.media_7)
    );
  }

  // ...but the extension only decides when the TYPE was vague. A real
  // non-media type still wins, so a .zip cannot get in by being named .mp3.
  {
    const t = build();
    await tick(2);
    t.respond({ url: "https://files.example/archive.mp3", mime: "application/zip", type: "main_frame" });
    await tick(6);
    check(
      "sniffer: a real non-media type still wins over the extension",
      (t.store.media_7 || []).length === 0,
      JSON.stringify(t.store.media_7)
    );
  }

  // Stream segments must still be kept out, or a playlist floods the list.
  {
    const t = build();
    await tick(2);
    t.respond({ url: "https://cdn.example/hls/seg9.ts", mime: "video/mp2t" });
    await tick(6);
    check(
      "sniffer: stream segments are still excluded",
      (t.store.media_7 || []).length === 0,
      JSON.stringify(t.store.media_7)
    );
  }

  // A short notification blip is not a download worth offering.
  {
    const t = build();
    await tick(2);
    t.respond({ url: "https://cdn.example/blip.mp3", mime: "audio/mpeg", size: 4096 });
    await tick(6);
    check(
      "sniffer: a tiny UI sound stays out",
      (t.store.media_7 || []).length === 0,
      JSON.stringify(t.store.media_7)
    );
  }
}

// ------------------------------------- 9. naming a captured media file
//
// A CDN that stores its objects under a uuid hands the user a row of
// gibberish to rename by hand; the page title is what IDM gives them and
// what they asked for.
{
  const uuid = "https://cdn.example/1b364999/fc1eced1-6d50-4375-a125-ef65c887d7d5.mp4";
  const h = build();
  await tick(4);
  h.respond({ url: uuid, mime: "video/mp4", size: 109_566_922 });
  await tick(6);
  await h.send({ type: "download-url", url: `${uuid}?e=1&s=abc`, referer: "https://page.example/watch" });
  await tick(2);
  const dl = h.sent.find((m) => m.type === "download");
  check(
    "naming: an opaque CDN name becomes the page title",
    dl?.filename === "Sunset Timelapse 4K.mp4",
    JSON.stringify(dl)
  );
  check("naming: the referer travels with it", dl?.referer === "https://page.example/watch");

  // A file that names itself keeps its name: a page of samples would
  // otherwise put the same title on every one of them.
  const h2 = build();
  await tick(4);
  h2.respond({ url: "https://cdn.example/big_buck_bunny_1080p.mp4", mime: "video/mp4" });
  await tick(6);
  await h2.send({ type: "download-url", url: "https://cdn.example/big_buck_bunny_1080p.mp4" });
  await tick(2);
  check(
    "naming: a file that names itself is left alone",
    !h2.sent.find((m) => m.type === "download")?.filename,
    JSON.stringify(h2.sent)
  );

  // An ordinary link is not media the page played, so the article's title
  // has nothing to do with it.
  const h3 = build();
  await tick(4);
  await h3.send({ type: "download-url", url: "https://files.example/8675309.zip" });
  await tick(2);
  check(
    "naming: an unsniffed link keeps its own name",
    !h3.sent.find((m) => m.type === "download")?.filename,
    JSON.stringify(h3.sent)
  );
}

console.log(fails ? `\n${fails} FAILED` : "\nall passed");
process.exit(fails ? 1 : 0);
