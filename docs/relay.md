# Push relay

Baton's Mac daemon sends "a session needs you" / "finished" notifications
to the user's iPhone through a small relay: a Cloudflare Worker (`relay/`)
that holds the APNs key. Payloads are end-to-end encrypted from the daemon
to the phone. The relay sees only ciphertext and stores nothing except
short-lived rate-limit counters keyed by hashes. We host one, and anyone
can run their own (`relay/README.md`).

```
 iPhone app ──(1) POST /v1/register {token, env}──────────────▶ relay ──▶ APNs
     ▲  ◀──────(2) background push {credential}───────────────────────────┘
     │
     └─(3) POST /api/push/register {token, public_key, env, credential}──▶ Mac daemon
                                                                          │
 iPhone NSE ◀── APNs ◀── relay ◀──(4) POST /v1/send {token, env, credential, ciphertext}
   decrypts                        (HPKE-sealed to public_key)
```

## What each party learns

| Party | Sees |
|---|---|
| Relay (and Cloudflare) | device token, APNs environment, ciphertext length, timing, the Mac's IP address, an opaque collapse id |
| Apple | the same, minus the Mac's IP address |
| Daemon | everything (it writes the notifications) |
| Phone | everything |

The relay never sees titles, bodies, session ids, project names or the
device public key. Collapse ids are hashes keyed by the device public key,
so the relay can't brute-force session names from them.

## Cryptography

**HPKE** (RFC 9180), base mode, suite
`DHKEM(X25519, HKDF-SHA256) / HKDF-SHA256 / ChaCha20-Poly1305`
(KEM 0x0020, KDF 0x0001, AEAD 0x0003).

- Rust: the [`hpke`](https://crates.io/crates/hpke) crate 0.13 (`crates/baton-daemon/src/push.rs`).
- Swift: CryptoKit `HPKE.Ciphersuite.Curve25519_SHA256_ChachaPoly`
  (iOS 17 / macOS 14) (`apple/PushCrypto`).
- `info` = `"baton push v1"` (ASCII); `aad` = empty.
- The device key is a raw 32-byte X25519 key (`Curve25519.KeyAgreement`
  `rawRepresentation`), sent to the daemon as standard base64.
- Every message uses a fresh ephemeral key. Wire format:
  `base64(enc[32] || ciphertext)`, with `ciphertext` = AEAD output
  including the 16-byte tag.

HPKE was chosen over a hand-assembled X25519 + HKDF + ChaCha20-Poly1305
because both platforms ship it natively, it has published test vectors,
and it leaves no construction details (KDF labels, nonce derivation) for
us to get wrong.

**Plaintext** (`push::Notification`) is compact JSON, padded with trailing
spaces to a multiple of 64 bytes so that ciphertext length reveals little.
Bodies are truncated to keep the plaintext at 1536 bytes or less.

```json
{"v":1,"kind":"needs_permission","title":"baton needs permission",
 "body":"Claude needs your permission to use Bash","session":"4f1c…","ts":1790000000}
```

`kind` is one of `needs_permission`, `needs_input` or `finished` (the
daemon maps Claude Code `Notification` / `Stop` hook events; see
`push::from_hook_event`). The phone ignores unknown fields and rejects a
`v` other than 1.

**Test vectors** live in `crates/baton-daemon/testdata/push_vectors.json`.
Both sides test against them:

- `rfc9180_a2_1` is RFC 9180 Appendix A.2.1. Rust reproduces `enc` and
  `ct` exactly from `ikmE`/`ikmR`; CryptoKit decrypts it. This shows that
  both libraries implement the same suite.
- `baton_v1` is a real Baton notification sealed by the daemon code with a
  fixed ephemeral seed. Rust asserts the exact plaintext bytes and the
  sealed output; Swift opens `sealed_b64` to the same plaintext bytes,
  both directly and through the APNs `userInfo` path the extension uses.
  CryptoKit can't fix the ephemeral key, so the Swift side checks
  decryption, the direction that matters, and its own random-key
  roundtrip.

## Pairing (registration)

1. The app creates an X25519 keypair on first launch and stores the
   private key in a Keychain access group shared with the Notification
   Service Extension (`kSecAttrAccessibleAfterFirstUnlock`, so the NSE can
   read it while the phone is locked).
2. The app gets its APNs device token and calls the relay:
   `POST {relay}/v1/register {"device_token": "<hex>", "env": "sandbox"|"production"}`.
3. The relay replies `202 {"ok":true,"delivery":"push"}` and sends a
   **background push** to that token:
   `{"aps":{"content-available":1},"baton":{"v":1,"type":"register","env":"…","credential":"<43 chars>"}}`.
   The credential is never in the HTTP response. Only the device that
   actually receives pushes for the token can learn it.
4. The app (in the foreground during pairing) receives it in
   `application(_:didReceiveRemoteNotification:fetchCompletionHandler:)`
   (`PushCrypto.registration(fromUserInfo:)`) and calls the daemon over
   the existing authenticated API:
   `POST /api/push/register {"device_token","public_key","env","credential","name"?}`.
   If the push hasn't arrived within about 20 s, retry (registration is
   rate-limited to 5 per token per hour).
5. The app should re-register whenever the device token changes, and
   when the daemon or relay reports `bad_credential` (for example after the
   relay rotated its secret).

### Daemon endpoints (bearer-authenticated like the rest of `/api/`)

- `POST /api/push/register {device_token, public_key, env?, credential, name?}`
  → `200 {"ok":true,"devices":N,"relay":"<url>"|null}`; `400 {"error":…}` on
  validation errors. `env` defaults to `production`. Re-registering a token
  replaces its entry.
- `POST /api/push/unregister {device_token}` → `200 {"ok":true,"removed":bool}`.

The device store is a JSON file,
`$BATON_PUSH_DEVICES_FILE` or `~/.config/baton/push-devices.json`, written
atomically with mode 0600 because it holds relay credentials.

## Sending

For each notification, the daemon seals the plaintext to each registered
device and posts:

```
POST {relay}/v1/send
{"device_token":"<hex>","env":"sandbox"|"production","credential":"…",
 "ciphertext":"<base64(enc||ct)>","collapse_id":"<opaque>"?,
 "priority":"high"|"normal","ttl":3600?}
```

| Relay response | Meaning | Daemon action |
|---|---|---|
| `200 {"ok":true,"apns_id"}` | delivered to APNs | none |
| `410 {"error":"unregistered"}` | APNs: `Unregistered` / `BadDeviceToken` / `DeviceTokenNotForTopic` | drop the device |
| `403 {"error":"bad_credential"}`, `401` | credential invalid | keep; the phone must re-register |
| `429` (+ `retry-after`) | relay or APNs rate limit | skip this one |
| `400` | malformed request | bug; logged |
| `502 {"error":"apns_error"}`, `503 relay_not_configured` | relay-side failure | logged |

`priority` maps to `apns-priority` 10 or 5. `ttl` (0 to 86400 s, default
3600) sets `apns-expiration`, where 0 means deliver now or drop.
`collapse_id` (64 bytes or less) replaces an earlier notification for the
same session and kind on the phone.

## Delivery (relay → APNs → phone)

The relay sends an alert push:

```json
{"aps":{"alert":{"title":"Baton","body":"New activity"},"mutable-content":1,"sound":"default"},
 "baton":{"v":1,"ct":"<ciphertext as received>"}}
```

with `apns-push-type: alert`, `apns-topic: $APNS_TOPIC`, and the
priority, collapse id and expiration from the request. The Notification
Service Extension decrypts `baton.ct` and replaces the title and body. If
it can't (key missing, 30 s budget exceeded), the phone shows the generic
"Baton / New activity", which leaks nothing.

The APNs provider token is an ES256 JWT (`kid` = key id, `iss` = team id,
`iat`), signed in the Worker with WebCrypto from the `.p8` secret, cached
per isolate and refreshed every 40 minutes (APNs rejects tokens older than
60 minutes and throttles refreshes more often than every 20). If APNs
answers `ExpiredProviderToken`, the relay re-signs and retries once.

## Abuse control

The relay must not be an open APNs cannon.

- **Send credentials.** `/v1/send` requires
  `credential = base64url(HMAC-SHA256(REGISTRATION_SECRET, "baton-relay/v1|<env>|<token>"))`.
  The relay verifies it statelessly, with a constant-time WebCrypto
  `verify`. A credential is bound to one token and one environment, and it
  is only ever delivered by push to that token. Knowing a device token is
  therefore not enough to send to it: you also need to be able to read
  that device's pushes.
- **Registration** can make the relay push only a silent background
  notification (no alert, nothing visible) to a token. It is rate-limited
  to 5 per token per hour and 30 per client IP per hour.
- **Sends** are rate-limited to 20 per minute and 300 per hour per
  (env, token). Invalid credentials are rejected before any Durable Object
  or APNs work.
- **Fixed topic.** The relay only ever uses `APNS_TOPIC`, so a team-wide
  APNs key can't be used to reach the team's other apps.
- **Size limits.** Request bodies are capped at 8 KiB, ciphertext at 48 to
  2048 bytes, and device tokens must be 64 to 200 hex characters.
- **Nothing stored.** Rate limits are one Durable Object per SHA-256 of
  the key (never a raw token or IP). They hold `{windowStart, count}` per
  rule and an alarm wipes them once the windows expire.
- **Revocation.** Rotating `REGISTRATION_SECRET` invalidates every
  credential at once, and phones re-register on `bad_credential`. There is
  no per-device revocation at the relay; to drop one device, the daemon
  removes it from its store.
- **What a leaked credential allows.** Up to the rate limit, someone
  holding it can send pushes to that one device that show "Baton / New
  activity" (the NSE can't decrypt them). They can't read anything or send
  readable text.

## Environment and topic

`env` (`sandbox` for development builds, `production` for TestFlight and
the App Store) is chosen per registration: the app knows it from its
`aps-environment` entitlement. It is stored with the device and sent on
every request. The relay selects `api.sandbox.push.apple.com` or
`api.push.apple.com`, and the credential is bound to the environment. The
topic (bundle id) is relay configuration, not a request field.

## Self-hosting

The daemon takes the relay URL from `BATON_RELAY_URL`, or from
`baton serve --relay URL`, or from `relay=URL` in the `--config` file (in
that order of precedence). With no relay URL, push is disabled and
registrations are still stored. A self-hosted relay needs its own APNs key
and the app built with the matching bundle id and team, because APNs only
delivers to apps of the key's team. See `relay/README.md`.

## iOS integration (later)

1. Add `apple/PushCrypto` as a local package to the app and to a new
   Notification Service Extension target.
2. Capabilities: Push Notifications (`aps-environment`), Background Modes
   → Remote notifications (for the registration push), and Keychain
   Sharing / App Group shared by the app and the NSE.
3. App: create or load the key (`PushCrypto.generatePrivateKey()`, store
   `rawRepresentation` in the shared keychain group), call
   `registerForRemoteNotifications()`, and run the pairing flow above. Send
   `PushCrypto.publicKeyBase64(key)` as `public_key`.
4. NSE (`didReceive(_:withContentHandler:)`): load the key, then
   `let n = try PushCrypto.notification(fromUserInfo: request.content.userInfo, key: key)`.
   Set `content.title = n.title`, `content.body = n.body`,
   `content.threadIdentifier = n.session ?? ""` and a category per `n.kind`
   (for example actions for `needs_permission`). On any error, deliver the
   content unchanged (the generic fallback). Implement
   `serviceExtensionTimeWillExpire` to deliver the fallback.
5. Handle `bad_credential` from a test send by re-running registration.

## Integration (daemon wiring, to apply after the in-flight merges)

`push.rs` is not wired into the binary yet. Until it is,
`crates/baton-daemon/tests/push.rs` compiles it via `#[path]` so that
`cargo test -p baton-daemon push` runs its tests. After applying the diff
below, delete `tests/push.rs` (the same tests then run as unit tests).
Verified on a scratch copy of this branch: `cargo clippy --all-targets -D
warnings` is clean, all daemon tests pass, and a live `baton serve`
answered `/api/push/register` and `/api/push/unregister` (401 without the
bearer token).

The diff adds the route, the state field, `--relay`, and the hook-event →
push forwarder. Apply it with `git apply`:

```diff
diff --git a/crates/baton-daemon/src/main.rs b/crates/baton-daemon/src/main.rs
--- a/crates/baton-daemon/src/main.rs
+++ b/crates/baton-daemon/src/main.rs
@@ -7,6 +7,7 @@ mod cmd;
 mod http;
 mod logs;
 mod proxy;
+mod push;
 mod pyjson;
 mod state;
 
@@ -17,7 +18,7 @@ use std::net::SocketAddr;
 use std::sync::Arc;
 use tokio::net::TcpListener;
 
-const USAGE: &str = "usage: baton serve [--listen 127.0.0.1:19880] [--upstream http://127.0.0.1:19877] [--config FILE]\n       baton sessions   (session list as JSON)";
+const USAGE: &str = "usage: baton serve [--listen 127.0.0.1:19880] [--upstream http://127.0.0.1:19877] [--relay URL] [--config FILE]\n       baton sessions   (session list as JSON)";
 
 fn flag(args: &[String], name: &str) -> Option<String> {
     args.iter()
@@ -89,9 +90,15 @@ async fn serve(args: &[String]) {
         .parse()
         .expect("--listen host:port");
     let upstream_url = arg(args, "--upstream", "http://127.0.0.1:19877");
-    let state = Arc::new(state::State::new(proxy::Upstream::new(&upstream_url)));
+    // Push relay: BATON_RELAY_URL, else --relay / `relay=` in --config.
+    let relay = Some(arg(args, "--relay", "")).filter(|r| !r.is_empty());
+    let state = Arc::new(state::State::new(
+        proxy::Upstream::new(&upstream_url),
+        relay,
+    ));
     let listener = TcpListener::bind(listen).await.expect("bind");
     tokio::spawn(api::list::prewarm(state.clone()));
+    tokio::spawn(state.push.clone().run(state.events.subscribe()));
     eprintln!(
         "baton serve on {listen} (upstream {upstream_url}, auth {})",
         if state.token.is_empty() { "off" } else { "on" }
diff --git a/crates/baton-daemon/src/state.rs b/crates/baton-daemon/src/state.rs
--- a/crates/baton-daemon/src/state.rs
+++ b/crates/baton-daemon/src/state.rs
@@ -25,14 +25,17 @@ pub struct State {
     /// list watcher skips rebuilds for 3 s after it (they compete with the
     /// terminal for CPU) — same rule as the Python server.
     pub last_input_ms: AtomicU64,
+    /// Push devices + relay client (push.rs, docs/relay.md).
+    pub push: std::sync::Arc<crate::push::Push>,
 }
 
 impl State {
-    pub fn new(upstream: crate::proxy::Upstream) -> Self {
+    pub fn new(upstream: crate::proxy::Upstream, relay: Option<String>) -> Self {
         let home = std::env::var_os("HOME")
             .map(PathBuf::from)
             .unwrap_or_default();
         let (events, _) = tokio::sync::broadcast::channel(256);
+        let push = std::sync::Arc::new(crate::push::Push::from_env(&home, relay));
         State {
             token: load_token(&home),
             dirs: Dirs::default_home(),
@@ -41,6 +44,7 @@ impl State {
             upstream,
             events,
             last_input_ms: AtomicU64::new(0),
+            push,
         }
     }
 
diff --git a/crates/baton-daemon/src/api/mod.rs b/crates/baton-daemon/src/api/mod.rs
--- a/crates/baton-daemon/src/api/mod.rs
+++ b/crates/baton-daemon/src/api/mod.rs
@@ -10,13 +10,18 @@ pub mod tmux;
 
 use crate::proxy::Body;
 use crate::state::State;
+use http_body_util::BodyExt;
 use hyper::body::Incoming;
-use hyper::{Request, Response};
+use hyper::{Method, Request, Response, StatusCode};
 use std::sync::Arc;
 
 pub type Routed = Result<Response<Body>, Request<Incoming>>;
 
 pub async fn route(state: &Arc<State>, req: Request<Incoming>) -> Routed {
+    let req = match push(state, req).await {
+        Ok(r) => return Ok(r),
+        Err(req) => req,
+    };
     let req = match list::route(state, req).await {
         Ok(r) => return Ok(r),
         Err(req) => req,
@@ -31,3 +36,22 @@ pub async fn route(state: &Arc<State>, req: Request<Incoming>) -> Routed {
     };
     control::route(state, req).await
 }
+
+/// `POST /api/push/register` / `POST /api/push/unregister` (docs/relay.md).
+async fn push(state: &Arc<State>, req: Request<Incoming>) -> Routed {
+    let path = req.uri().path();
+    if req.method() != Method::POST
+        || !matches!(path, "/api/push/register" | "/api/push/unregister")
+    {
+        return Err(req);
+    }
+    let register = path == "/api/push/register";
+    let headers = req.headers().clone();
+    let (status, body) = match req.into_body().collect().await {
+        Ok(b) if register => state.push.register(&b.to_bytes()),
+        Ok(b) => state.push.unregister(&b.to_bytes()),
+        Err(_) => (400, serde_json::json!({"error": "unreadable body"})),
+    };
+    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
+    Ok(crate::http::json(status, &body, &headers))
+}
```

If the other merges change `State::new`'s callers or `api::route`, the
intent is: (1) `mod push;`, (2) a `push: Arc<push::Push>` field built with
`Push::from_env(&home, relay_url)`, (3) the two POST routes calling
`state.push.register` / `state.push.unregister`, and (4)
`tokio::spawn(state.push.clone().run(state.events.subscribe()))` at
startup. Hook events already reach `state.events` through
`/api/hook-event`, so no change to `control.rs` is needed.

## Open items

- **HTTP/2 from Workers to APNs.** APNs only speaks HTTP/2. The code uses
  plain `fetch()`, and the tests mock APNs, so this path has not been
  tested against Apple. Confirm it with the first sandbox send after
  deploying (see `relay/README.md`, "First deploy check"). If Workers turn
  out to use HTTP/1.1 for this origin, the fallback is to run the same
  handler on another runtime; the protocol doesn't change.
- **Background push delivery.** iOS may throttle background pushes, but
  they are delivered to a foreground app, which is when pairing happens.
  If this proves flaky, the registration push can become an alert push
  that the NSE consumes silently.
- **Minimum OS.** CryptoKit HPKE needs iOS 17 or later.
