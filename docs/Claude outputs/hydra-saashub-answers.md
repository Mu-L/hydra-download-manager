# SaaSHub profile answers — Hydra Download Manager (HDM)

Paste each block into the matching question form.

---

## 1. What makes Hydra by javad.dev unique?

Two things, mainly.

**It is a download manager and an embeddable engine at the same time.** The scheduler that powers the desktop app and the CLI also ships as `libhydra` — a frozen C ABI, one static/shared library and one header (`hydra.h`) — so you can put multi-source downloading inside your own application without bundling a download manager next to it. There are drop-in examples for C, C++, Go, Python, Swift, Kotlin, Dart, C# and Zig, plus prebuilt Android `jniLibs` and an Apple `.xcframework` with background-execution policy support. Most tools in this category are either an application or a library. Hydra is deliberately both, and the licensing is split to make that usable rather than nominal: GPL-3.0-or-later on the `hydra` binary, MIT/Apache-2.0 on the engine and the FFI layer, with a hard rule that the embeddable crate never depends on the copyleft ones.

**The scheduler does more than open N connections and hope.** It rebalances while the transfer is running. When a connection starts lagging mid-flight, Hydra reclaims its unfinished byte ranges and reassigns them to faster peers instead of waiting on the slow one — and it detects the collapse early, using statistical CUSUM estimators that flag a degrading connection seconds before a socket timeout would fire. Writes go straight to positioned offsets on disk instead of through RAM buffers, so memory stays flat: about 7 MiB for a 1 GB transfer, and it does not grow with file size.

That shows up in the numbers. On a controlled 100 ms path (1 GB served from RAM by nginx in a network namespace, 50 ms `tc netem` each way, mean of three runs), Hydra reaches 414 MB/s at `-x 8` against aria2c's 298 MB/s at the same concurrency — at roughly a third of the memory and two thirds of the CPU. A bare `hydra <url>` with no flags matches `aria2c -x 4`. The benchmark scripts are in the repository, so none of this has to be taken on trust.

---

## 2. Why should a person choose Hydra by javad.dev over its competitors?

It depends which tool you are coming from.

**From aria2** — Hydra is faster at every matched concurrency level in our benchmarks, uses about a third of the memory and two thirds of the CPU, and issues exactly one request per connection with no repairs. It also gives you things aria2 does not: a native desktop GUI, browser extensions for Chrome, Edge, Firefox and Safari, and an interactive TUI for managing the queue.

**From Internet Download Manager** — Hydra is free and open source, runs on Windows, macOS and Linux rather than Windows alone, and there is no licence key or trial. In a same-network 100 MB desktop test it finished in 13.0 s against IDM's 18.3 s.

**From Free Download Manager, JDownloader or ABDownloadManager** — the difference you will actually feel is resource use. In that same desktop test Hydra's GUI peaked at 39 MiB of memory; Free Download Manager used 352 MiB and ABDownloadManager 501 MiB, both for a slower download.

**From wget or curl** — the CLI is flag-compatible with both, so it slots into existing scripts, and you get multi-connection and multi-mirror acceleration without rewriting anything.

Three things apply whichever tool you are replacing. There is **no telemetry** — no analytics, no crash reporting, no account system, and no server for the project to run; the browser extension only talks to the app on your own machine over loopback. There is **real integrity checking** — per-chunk checksum manifests, verification against server-advertised digests like Content-MD5, and optional Reed–Solomon parity against bitrot. And the **engine is embeddable**, so if you later want this behaviour inside your own product, you are not starting over.

---

## 3. How would you describe the primary audience of Hydra by javad.dev?

Three groups, in concentric circles.

**People moving large files over connections that are not perfect.** Anyone downloading multi-gigabyte files — ISOs, game assets, datasets, media archives — on a link that is slow, capped, high-latency, or prone to dropping. This is where range stealing and stall recovery pay for themselves, and it includes a lot of users in regions where a stalled download means starting over. Many of them arrive looking for a free, cross-platform alternative to IDM.

**Terminal and Linux users.** The CLI is `wget`/`curl` flag-compatible and available as `hydra` or the shorter `hya`, so it drops into existing scripts and CI jobs. It ships through Homebrew, an Ubuntu PPA, Fedora COPR, the AUR, `.deb`/`.rpm` packages and a self-updating AppImage. The interactive TUI covers queue management without leaving the shell.

**Developers who need downloading inside their own software.** This is the `libhydra` audience — mobile and desktop app developers, Flutter and Go and Swift projects, anyone shipping a client that has to fetch large files reliably. They get a stable C ABI with a documented compatibility policy, CI-enforced, under MIT/Apache-2.0 so it can be linked into closed-source products.

The common thread is that all three care about what happens when a download goes wrong, not just how fast it goes when everything is fine.

---

## 4. What's the story behind Hydra by javad.dev?

Hydra started from a narrow observation: most download accelerators treat parallelism as a fixed decision made at the start. You pick a connection count, the file is cut into that many pieces, and if one of those connections turns out to be slow the whole transfer waits on it. The work is partitioned once and never revisited.

The project began as an attempt to make that decision continuous instead — to keep measuring every connection and move work away from the ones that are failing, while they are still failing rather than after a timeout admits it. That is where range stealing and the CUSUM-based stall detector come from, and it is why the scheduler was built as an I/O-free crate that can be tested and benchmarked on its own.

The rest followed from taking that engine seriously. A benchmark suite came early, because a claim about a scheduler is worth nothing without a reproducible way to check it — the harness and the scripts ship in the repository, and the published results include the cases where Hydra does not win. The CLI came first and was made `wget`/`curl` compatible so it could be adopted without rewriting anything. The desktop GUI and the browser extensions followed, for people who were never going to use a terminal. And `libhydra` came last, once it was clear the engine was more broadly useful than the application wrapped around it.

That last step forced the licensing question the project is probably most opinionated about. Rust links statically, so there is no dynamic-linking boundary of the kind the LGPL was written for: a copyleft engine would relicense every application that embedded it. So the licences are split by role rather than applied uniformly — GPL-3.0-or-later on the binary, where copyleft costs nothing because nobody links a CLI, and MIT/Apache-2.0 on the engine and the C ABI, whose entire purpose is being linked into someone else's code. It is a deliberate choice, written down and explained in the repository, and the dependency graph is arranged so it cannot be broken by accident.

Hydra is open source and developed in the open at github.com/ja7ad/hydra.

---

## 5. Which are the primary technologies used for building Hydra by javad.dev?

**Core language:** Rust (2021 edition), organised as an eight-crate Cargo workspace — engine, transport, streaming, FFI, CLI, GUI, native-messaging host and updater — with each crate's licence and dependency direction fixed by role.

**Async runtime and networking:** Tokio for the multi-threaded runtime, and rustls with the `ring` provider for TLS (chosen over the default so the build stays offline-friendly and needs no C toolchain), plus `webpki-roots`. Protocol support covers HTTP/S and FTP, with HTTP CONNECT tunnelling and SOCKS4/4a/5 proxies.

**Integrity:** BLAKE3 for per-chunk manifests, `reed-solomon-simd` over GF(2¹⁶) for at-rest parity, and SHA-2, SHA-1 and MD5 for verifying the digests servers actually advertise (Content-MD5, `x-goog-hash`, Metalink documents).

**Desktop GUI:** `iced` 0.14 on winit — pure Rust, no GTK or Electron — with SVG icon assets that stay crisp at any DPI, native tray integration via `tray-icon` on Windows and macOS and `ksni` on Linux.

**CLI and TUI:** `clap` for argument parsing with `clap_complete` generating shell completions for bash, zsh, fish, elvish and PowerShell from the same command definition, and `crossterm` for the interactive queue manager.

**Browser integration:** extensions for Chrome, Edge, Firefox and Safari, talking to the desktop app over a local WebSocket bridge (RFC 6455) with a native-messaging fallback that can launch the app on demand.

**Embedding layer:** `libhydra`, a frozen C ABI with a single header, distributed as static and shared libraries with ABI-stability enforced in CI, plus Android `jniLibs` and an Apple `.xcframework`.

**Streaming:** HLS and DASH manifest parsing and segment assembly, including AES-128-CBC segment decryption using audited `aes`/`cbc` implementations.

**Build, test and distribution:** GitHub Actions CI with Codecov coverage reporting, Criterion for benchmarks, and packaging for Homebrew, Ubuntu PPA, Fedora COPR, Arch AUR, `.deb`, `.rpm`, Windows installer, macOS `.app`/`.pkg`/`.dmg` and a self-updating AppImage.

---

## 6. Who are some of the biggest customers of Hydra by javad.dev?

Hydra is a free and open-source project rather than a commercial product, so it has no customers in the usual sense — there is no account system, no licence server, and nothing to sign up for.

It also collects no telemetry of any kind. There is no analytics, no crash reporting, and no server that the desktop app or the browser extension ever contacts, which means the project genuinely has no way to know who is running it. That is a deliberate design choice rather than a gap.

What can be pointed to is distribution: Hydra is published on crates.io, Homebrew, an Ubuntu PPA, Fedora COPR, the Arch User Repository, and as `.deb`, `.rpm`, Windows, macOS and AppImage builds, alongside browser extensions on the Firefox and Chrome stores. The engine is also available as `libhydra` under MIT/Apache-2.0 for embedding into other applications.
