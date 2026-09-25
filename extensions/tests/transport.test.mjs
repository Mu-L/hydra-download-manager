// Copyright (C) 2026 Javad Rajabzadeh
// SPDX-License-Identifier: GPL-3.0-or-later
//
// The transport under adversity: a socket the OS dropped without saying so,
// a native host that launched the app but could not reach it in time, and
// the token handshake that opens every socket — cached, stale, or with no
// host to fetch one from.
// Timers are virtual, so a 20 s heartbeat and a 60 s hold cost nothing.
// Run from the repository root:  node extensions/tests/transport.test.mjs
import vm from "node:vm";
import { loadBackground } from "./load.mjs";

let fails = 0;
const check = (l, c, x = "") => { if (c) console.log(`ok   ${l}`); else { fails++; console.log(`FAIL ${l} ${x}`); } };
const tick = (n = 6) => new Promise((r) => { let i = 0; const f = () => (++i >= n ? r() : setImmediate(f)); setImmediate(f); });

/// A clock the test moves by hand. Timer callbacks run in due order; the
/// promise machinery underneath them is still the real one.
function fakeTimers() {
  let now = 0, seq = 1;
  const timers = new Map();
  const add = (fn, ms, every) => { const id = seq++; timers.set(id, { fn, at: now + Math.max(0, ms || 0), every }); return id; };
  return {
    setTimeout: (fn, ms) => add(fn, ms, null),
    setInterval: (fn, ms) => add(fn, ms, ms),
    clearTimeout: (id) => timers.delete(id),
    clearInterval: (id) => timers.delete(id),
    now: () => now,
    async advance(ms) {
      const until = now + ms;
      for (;;) {
        const due = [...timers.entries()].filter(([, t]) => t.at <= until).sort((a, b) => a[1].at - b[1].at)[0];
        if (!due) break;
        const [id, t] = due;
        now = t.at;
        if (t.every) t.at = now + t.every; else timers.delete(id);
        t.fn();
        await tick();
      }
      now = until;
      await tick();
    },
  };
}

/// `token` is what the fake app accepts on `auth`; `cachedToken` is what the
/// worker finds in session storage (null: nothing cached); `allowlisted`
/// makes the app admit the socket without any token, as it does for the
/// extension ids it ships under.
function build({ wsOpens = true, pings = true, native = () => undefined, token = "tok", cachedToken = "tok", allowlisted = false } = {}) {
  const clock = fakeTimers();
  const sent = [], calls = [], sockets = [];
  const store = cachedToken ? { ws_token: cachedToken } : {};
  const items = new Map();
  const ev = () => { const l = []; return { addListener: (f) => l.push(f), fire: (...a) => l.map((f) => f(...a)), l }; };
  const mk = () => ({
    async get(d) { if (typeof d === "string") return { [d]: store[d] }; const o = {}; for (const [k, v] of Object.entries(d)) o[k] = k in store ? store[k] : v; return o; },
    async set(p) { Object.assign(store, p); },
    async remove(k) { for (const x of [].concat(k)) delete store[x]; },
  });
  const onCreated = ev(), onDetermining = ev();
  const chrome = {
    runtime: {
      id: "hydra-ext", lastError: null, onInstalled: ev(), onStartup: ev(), onMessage: ev(), getURL: (p) => p,
      sendNativeMessage: (_h, m, cb) => { sent.push({ via: "host", ...m }); cb(native(m)); },
    },
    storage: { local: mk(), session: mk() },
    action: { setBadgeBackgroundColor: async () => {}, setBadgeText: async () => {}, setTitle: async () => {} },
    downloads: {
      onCreated, onDeterminingFilename: onDetermining,
      pause: async (id) => { calls.push(["pause", id]); items.get(id).paused = true; },
      resume: async (id) => { calls.push(["resume", id]); items.get(id).paused = false; },
      cancel: async (id) => { calls.push(["cancel", id]); items.get(id).state = "interrupted"; },
      erase: async (q) => calls.push(["erase", q.id]),
      search: async ({ id }) => (items.has(id) ? [items.get(id)] : []),
    },
    contextMenus: { removeAll: async () => {}, create: () => {}, onClicked: ev() },
    tabs: { query: async () => [], get: async () => null, sendMessage: async () => ({}), create: () => {}, onRemoved: ev(), onUpdated: ev() },
    cookies: { getAll: async () => [] },
    webRequest: { onResponseStarted: { addListener: () => {} } },
  };
  class WS {
    constructor(url) {
      sockets.push(this);
      this.url = url;
      this.closed = false;
      this.authed = allowlisted;
      if (wsOpens) setImmediate(() => this.onopen && this.onopen());
    }
    open() { this.onopen && this.onopen(); }
    send(raw) {
      const m = JSON.parse(raw);
      sent.push({ via: "ws", ...m });
      const answer = (rep) => setImmediate(() => this.onmessage && this.onmessage({ data: JSON.stringify(rep) }));
      if (!this.authed) {
        // The app's gate: one `auth` with the right token, or a refusal and
        // the door shut behind it.
        if (m.type === "auth" && m.token === token) { this.authed = true; answer({ ok: true, id: m.id }); return; }
        answer({ ok: false, id: m.id, error: "unauthorized" });
        setImmediate(() => { this.closed = true; this.onclose && this.onclose(); });
        return;
      }
      if (m.type === "auth") return answer({ ok: true, id: m.id });
      if (m.type === "ping" && !pings) return; // the peer is gone; nothing comes back
      answer({ ok: true, id: m.id, capture: true, auto_types: "ZIP", dont_start_sites: "" });
    }
    close() { this.closed = true; }
  }
  const ctx = {
    chrome, navigator: { userAgent: "Chrome/120" }, WebSocket: WS, console: { ...console, debug: () => {} },
    setTimeout: clock.setTimeout, clearTimeout: clock.clearTimeout,
    setInterval: clock.setInterval, clearInterval: clock.clearInterval,
    setImmediate, fetch: async () => { throw new Error("no net"); }, AbortController, URL, URLSearchParams,
  };
  const vmctx = vm.createContext(ctx);
  // The worker reads the wall clock too; it follows the virtual one.
  const epoch = Date.now();
  vm.runInContext("Date", vmctx).now = () => epoch + clock.now();
  loadBackground(vmctx, "chrome");
  const download = (id, url) => {
    const item = { id, url, filename: url.split("/").pop(), mime: "application/zip", totalBytes: 1e6, state: "in_progress", paused: false };
    items.set(id, item);
    onCreated.fire(item);
    onDetermining.fire(item, () => calls.push(["suggest", id]));
    return item;
  };
  return { clock, sent, calls, sockets, items, download, store };
}

// ----------------------------------------------- 1. a silently dead socket
{
  const h = build({ pings: false });
  await tick();
  check("heartbeat: one socket is open", h.sockets.length === 1 && !h.sockets[0].closed);
  await h.clock.advance(20000);
  check("heartbeat: a ping went out on schedule", h.sent.some((m) => m.via === "ws" && m.type === "ping"), JSON.stringify(h.sent));
  check("heartbeat: an unanswered ping is still being waited on", !h.sockets[0].closed);
  await h.clock.advance(5000);
  check("heartbeat: the dead socket is closed once the ping times out", h.sockets[0].closed);
  await h.clock.advance(1000);
  check("heartbeat: and a fresh one is dialled", h.sockets.length === 2, String(h.sockets.length));
}

// ------------------------------------------- 2. a healthy socket is kept
{
  const h = build();
  await tick();
  await h.clock.advance(25000);
  check("heartbeat: an answered ping keeps the socket", h.sockets.length === 1 && !h.sockets[0].closed);
}

// ------------------------------------- 3. the host says the app is starting
{
  let hostReplies = { ok: false, error: "hydra is starting" };
  const h = build({ wsOpens: false, native: () => hostReplies });
  await tick();
  h.download(1, "https://cdn.example/big.zip");
  await tick(10);
  check("starting: the host was asked", h.sent.some((m) => m.via === "host" && m.type === "download"), JSON.stringify(h.sent));
  check("starting: the download stays parked", h.items.get(1).paused && !h.calls.some(([c]) => c === "resume"), JSON.stringify(h.calls));
  check("starting: the browser is released from target determination", h.calls.some(([c]) => c === "suggest"));

  // The app comes up: its socket attaches, and the parked download is
  // offered again over it.
  h.sockets[0].open();
  await tick(12);
  const again = h.sent.filter((m) => m.type === "download");
  check("starting: the download is offered again once the socket attaches", again.length === 2 && again[1].via === "ws", JSON.stringify(again));
  check("starting: Hydra took it — the browser's copy is cancelled and erased",
    h.calls.some(([c]) => c === "cancel") && h.calls.some(([c]) => c === "erase") && !h.calls.some(([c]) => c === "resume"), JSON.stringify(h.calls));

  // A second one after the socket is up never consults the host.
  h.download(2, "https://cdn.example/other.zip");
  await tick(10);
  check("starting: with the socket up the host is not asked", !h.sent.some((m) => m.via === "host" && m.url?.endsWith("other.zip")));
}

// -------------------------- 4. the app never comes up: the browser gets it
{
  const h = build({ wsOpens: false, native: () => ({ ok: false, error: "hydra is starting" }) });
  await tick();
  h.download(3, "https://cdn.example/big.zip");
  await tick(10);
  check("hold: the download is parked while the app starts", h.items.get(3).paused);
  await h.clock.advance(59000);
  check("hold: still parked a minute in", h.items.get(3).paused);
  await h.clock.advance(2000);
  check("hold: handed back to the browser when the app never appears", !h.items.get(3).paused && h.calls.some(([c, id]) => c === "resume" && id === 3), JSON.stringify(h.calls));
}

// ----------------- 5. "not running" is a refusal, not a reason to hold on
{
  const h = build({ wsOpens: false, native: () => ({ ok: false, error: "hydra is not running" }) });
  await tick();
  h.download(4, "https://cdn.example/big.zip");
  await tick(10);
  check("not running: the browser keeps the download at once", !h.items.get(4).paused && h.calls.some(([c, id]) => c === "resume" && id === 4), JSON.stringify(h.calls));
}

// -------------- 6. a download the user resumed meanwhile is left to them
{
  const h = build({ wsOpens: false, native: () => ({ ok: false, error: "hydra is starting" }) });
  await tick();
  const item = h.download(5, "https://cdn.example/big.zip");
  await tick(10);
  item.paused = false; // the user clicked resume in the downloads shelf
  h.sockets[0].open();
  await tick(12);
  check("hold: a download the user resumed is not offered again", h.sent.filter((m) => m.type === "download").length === 1 && !h.calls.some(([c]) => c === "cancel"), JSON.stringify(h.sent));
}

// ------------------------------ 7. a cached token opens the socket outright
{
  const h = build();
  await tick();
  const first = h.sent[0];
  check("auth: the first frame is auth with the cached token", first?.via === "ws" && first.type === "auth" && first.token === "tok", JSON.stringify(h.sent));
  check("auth: settings are synced once admitted", h.sent.some((m) => m.via === "ws" && m.type === "config"));
  check("auth: the host was never spawned", !h.sent.some((m) => m.via === "host"), JSON.stringify(h.sent));
  h.download(7, "https://cdn.example/big.zip");
  await tick(10);
  check("auth: captures go over the authenticated socket", h.sent.some((m) => m.via === "ws" && m.type === "download") && h.calls.some(([c]) => c === "cancel"), JSON.stringify(h.calls));
}

// ------------ 8. a stale token: refused, refreshed through the host, redialled
{
  const hostCalls = [];
  const h = build({
    cachedToken: "stale",
    native: (m) => { hostCalls.push(m.type); return m.type === "ws-token" ? { ok: true, ws_port: 6799, token: "tok" } : undefined; },
  });
  await tick(8);
  check("refresh: the stale token was refused", h.sent[0]?.type === "auth" && h.sent[0].token === "stale" && h.sockets[0].closed, JSON.stringify(h.sent));
  check("refresh: the host was asked for the current token", hostCalls.includes("ws-token"), JSON.stringify(hostCalls));
  check("refresh: and it is cached for the next worker", h.store.ws_token === "tok");
  check("refresh: no capture went over the refused socket", !h.sent.some((m) => m.via === "ws" && m.type === "config"));
  await h.clock.advance(300);
  check("refresh: the same port is redialled", h.sockets.length === 2 && h.sockets[1].url === h.sockets[0].url, JSON.stringify(h.sockets.map((s) => s.url)));
  const auths = h.sent.filter((m) => m.type === "auth");
  check("refresh: the fresh token is presented", auths.length === 2 && auths[1].token === "tok", JSON.stringify(auths));
  check("refresh: and the socket is live", h.sent.some((m) => m.via === "ws" && m.type === "config") && !h.sockets[1].closed);
  h.download(8, "https://cdn.example/big.zip");
  await tick(10);
  check("refresh: the capture went over the socket, not the host", h.sent.some((m) => m.via === "ws" && m.type === "download") && !hostCalls.includes("download"), JSON.stringify(hostCalls));
}

// ------- 9. no token and no host to ask: the native path, exactly as before
{
  const h = build({ cachedToken: null });
  await tick(8);
  check("no host: a tokenless auth was tried first", h.sent[0]?.type === "auth" && !("token" in h.sent[0]), JSON.stringify(h.sent[0]));
  check("no host: the refused socket is given up", h.sockets[0].closed && h.sockets.length === 1);
  check("no host: the host was asked and could not answer", h.sent.some((m) => m.via === "host" && m.type === "ws-token"));
  h.download(9, "https://cdn.example/big.zip");
  await tick(10);
  check("no host: the capture falls back to native messaging", h.sent.some((m) => m.via === "host" && m.type === "download"), JSON.stringify(h.sent));
  check("no host: and the browser keeps the download", !h.items.get(9).paused && h.calls.some(([c, id]) => c === "resume" && id === 9), JSON.stringify(h.calls));
  await h.clock.advance(29000);
  check("no host: the app is left alone meanwhile", h.sockets.length === 1);
  await h.clock.advance(2000);
  check("no host: and tried again later", h.sockets.length === 2, String(h.sockets.length));
}

// ------------------ 10. an allow-listed id is admitted with nothing cached
{
  const h = build({ cachedToken: null, allowlisted: true });
  await tick();
  check("allow-listed: admitted on a tokenless auth", h.sent[0]?.type === "auth" && h.sent.some((m) => m.type === "config"), JSON.stringify(h.sent));
  check("allow-listed: the host was never spawned", !h.sent.some((m) => m.via === "host"));
  await h.clock.advance(25000);
  check("allow-listed: the heartbeat keeps the socket", h.sockets.length === 1 && !h.sockets[0].closed);
}

console.log(fails ? `\n${fails} FAILED` : "\nall passed");
process.exit(fails ? 1 : 0);
