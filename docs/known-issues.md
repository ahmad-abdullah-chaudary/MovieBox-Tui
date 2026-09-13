# Known issues and limitations

Tracked here so future work and issue reports reference the same facts.

## Latent / by-design

- **`supports_headers` is compatibility policy, not just a parser guard.** Sources that
  carry authentication headers (e.g. MovieBox CloudFront signed cookies) exercise it for Android intent playback.
  Desktop VLC is now end-to-end header-capable: streams that need headers VLC cannot express
  in its CLI (the signed MovieBox `Cookie`) are played through a loopback HTTP relay that injects
  the headers and replays the manifest plus every relative segment URL. Android intent and players
  triggered through `termux-open`/`termux-open-url` support unauthenticated streams (CircleFTP, DhakaFlix, IPTV, direct streams)
  as well as streams with standard `referer`/`user-agent` headers (4KHDHub); cookie-authenticated
  Streams trip the compatibility gate on Android and guide the user to a header-capable player
  (`mpv`, or desktop `VLC` via the relay). Keep `player.rs::supports_headers` in sync with relay
  availability and Android opener capabilities.
- **BDIX clients use nested `if let Ok` pyramids** in search handling; they work and
  are logged, but are harder to read. Flattening is deferred (behavior-neutral refactor
  with moderate churn).
- **MovieBox request signing** hardcodes an API secret and spoofs a device identity in
  `crypto.rs`. This is inherent to the scraper; treat the module as one unit.
- **Android intent playback** forwards `User-Agent`, `Referer`, and subtitles (`subtitles_location`/`subs`) when using `am` or `termux-am`. Chooser-based launches via `termux-open` open the raw video URL directly.

## Environment-dependent

- **4KHDHub mirrors rotate and can be expired on upstream hosts.** Direct streams are resolved concurrently across all available release mirrors using prioritized scoring (Cloudflare R2 / S3 / Seekable Streams → Storage → PixelDrain API → Google UserContent / Direct Attachments) and bounded concurrency (`select_ok` in chunks of 3). When all upstream mirrors for an older release are dead/expired (e.g. 404 or expired tokens), the resolver fails fast (<4.5s) and guides the user to select another release.
- **Termux playback needs the device confirmed** on each release: `termux-open` /
  `am` availability and the Android chooser behavior. The historical
  `rustls-platform-verifier` initialization panic reported for v0.1.12 is not in
  the v0.1.13 dependency graph, but the upstream report remains open until a real
  Termux launch is observed.

## Verification

- Automated testing is enforced via `cargo test --all-features --locked` covering 346+ unit and integration tests across 21 test suites (see [`docs/testing.md`](testing.md)). The count is updated when tests change; it does not replace real-player and real-device verification.
- Static correctness is enforced by strict compiler type checking, the lint gate (`cargo clippy --all-targets --all-features --locked -- -D warnings`), formatting (`cargo fmt --check`), dependency vulnerability scanning (`cargo audit`), and packaging verification (`cargo package --locked`).
- Runtime and platform-specific behavior (terminal resize, focus handling, external player launch, and Termux chooser) are verified through the release checklist in [`release-checklist.md`](release-checklist.md).
