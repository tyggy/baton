//! Push notifications through the Baton relay (docs/relay.md).
//!
//! The iPhone registers `{device_token, public_key, env, credential}` with
//! the daemon (`POST /api/push/register`). For each notification the daemon
//! seals a small JSON document to the device's X25519 key with HPKE
//! (RFC 9180 base mode, DHKEM(X25519, HKDF-SHA256) / HKDF-SHA256 /
//! ChaCha20-Poly1305, the suite CryptoKit calls
//! `Curve25519_SHA256_ChachaPoly`) and posts only the ciphertext to the
//! relay, which forwards it to APNs. The relay never sees plaintext; the
//! iOS Notification Service Extension opens it.
//!
//! Environment: `BATON_PUSH_DEVICES_FILE` (device store, default
//! `~/.config/baton/push-devices.json`), `BATON_RELAY_URL` (relay base URL;
//! wins over the `relay=` key of `--config`). No relay URL = push disabled.
#![allow(dead_code)] // wired into main.rs/state.rs in a follow-up (docs/relay.md, Integration)

use base64::engine::general_purpose::{STANDARD as B64, URL_SAFE_NO_PAD as B64URL};
use base64::Engine;
use bytes::Bytes;
use hpke::{
    aead::ChaCha20Poly1305, kdf::HkdfSha256, kem::X25519HkdfSha256, Deserializable, Kem as _,
    OpModeR, OpModeS, Serializable,
};
use http_body_util::{BodyExt, Full};
use hyper_util::client::legacy::{connect::HttpConnector, Client};
use hyper_util::rt::TokioExecutor;
use rand_core::{CryptoRng, RngCore, TryRngCore};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

type Kem = X25519HkdfSha256;
type Kdf = HkdfSha256;
type Aead = ChaCha20Poly1305;

/// HPKE `info`: binds every message to this protocol and version.
pub const HPKE_INFO: &[u8] = b"baton push v1";
/// HPKE `aad`: unused (empty).
pub const HPKE_AAD: &[u8] = b"";
/// Encapsulated key length (X25519 public key).
pub const ENC_LEN: usize = 32;
/// Plaintexts are padded with trailing spaces (valid JSON whitespace) to a
/// multiple of this, so ciphertext length says little about the content.
pub const PAD_BLOCK: usize = 64;
/// The relay rejects larger ciphertexts; stay well under it.
pub const MAX_PLAINTEXT: usize = 1536;

#[derive(Debug, PartialEq, Eq)]
pub enum PushError {
    BadKey,
    Crypto,
    TooLarge,
    Io(String),
    Http(String),
    NoRelay,
}

impl std::fmt::Display for PushError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PushError::BadKey => write!(f, "bad device key"),
            PushError::Crypto => write!(f, "encryption failed"),
            PushError::TooLarge => write!(f, "notification too large"),
            PushError::Io(e) => write!(f, "device store: {e}"),
            PushError::Http(e) => write!(f, "relay: {e}"),
            PushError::NoRelay => write!(f, "no relay configured"),
        }
    }
}

// ── crypto ─────────────────────────────────────────────────────────────────

/// A fresh device keypair `(private, public)`, raw 32-byte X25519. The iOS
/// app makes its own with CryptoKit; this is for tests and tooling.
pub fn generate_keypair() -> ([u8; 32], [u8; 32]) {
    let (sk, pk) = Kem::gen_keypair(&mut rand_core::OsRng.unwrap_err());
    (to32(&sk.to_bytes()), to32(&pk.to_bytes()))
}

/// RFC 9180 DeriveKeyPair: the same `ikm` always gives the same keypair.
pub fn derive_keypair(ikm: &[u8]) -> ([u8; 32], [u8; 32]) {
    let (sk, pk) = Kem::derive_keypair(ikm);
    (to32(&sk.to_bytes()), to32(&pk.to_bytes()))
}

fn to32(b: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    out.copy_from_slice(b);
    out
}

/// Seal `plaintext` to the device key. Output is `enc (32) || ciphertext`,
/// which is what goes to the relay (base64) and on to the device.
pub fn seal(public_key: &[u8; 32], plaintext: &[u8]) -> Result<Vec<u8>, PushError> {
    seal_with_rng(
        public_key,
        plaintext,
        HPKE_INFO,
        HPKE_AAD,
        &mut rand_core::OsRng.unwrap_err(),
    )
}

/// `seal` with explicit `info`/`aad` and RNG. The RNG supplies the 32-byte
/// `ikmE` for the ephemeral key, so a fixed RNG reproduces the RFC 9180
/// test vectors exactly (known-answer tests).
pub fn seal_with_rng<R: CryptoRng + RngCore>(
    public_key: &[u8; 32],
    plaintext: &[u8],
    info: &[u8],
    aad: &[u8],
    rng: &mut R,
) -> Result<Vec<u8>, PushError> {
    let pk =
        <Kem as hpke::Kem>::PublicKey::from_bytes(public_key).map_err(|_| PushError::BadKey)?;
    let (enc, ct) =
        hpke::single_shot_seal::<Aead, Kdf, Kem, R>(&OpModeS::Base, &pk, info, plaintext, aad, rng)
            .map_err(|_| PushError::Crypto)?;
    let mut out = enc.to_bytes().to_vec();
    out.extend_from_slice(&ct);
    Ok(out)
}

/// Open `enc || ciphertext` with the device's private key (what the iOS
/// extension does; the daemon itself never decrypts).
pub fn open(
    private_key: &[u8; 32],
    sealed: &[u8],
    info: &[u8],
    aad: &[u8],
) -> Result<Vec<u8>, PushError> {
    if sealed.len() < ENC_LEN + 16 {
        return Err(PushError::Crypto);
    }
    let sk =
        <Kem as hpke::Kem>::PrivateKey::from_bytes(private_key).map_err(|_| PushError::BadKey)?;
    let enc = <Kem as hpke::Kem>::EncappedKey::from_bytes(&sealed[..ENC_LEN])
        .map_err(|_| PushError::Crypto)?;
    hpke::single_shot_open::<Aead, Kdf, Kem>(
        &OpModeR::Base,
        &sk,
        &enc,
        info,
        &sealed[ENC_LEN..],
        aad,
    )
    .map_err(|_| PushError::Crypto)
}

// ── notification ───────────────────────────────────────────────────────────

/// The plaintext the device decrypts. `kind` drives the UI (category,
/// sound); `title`/`body` replace the relay's generic "Baton" alert.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notification {
    pub v: u32,
    pub kind: String,
    pub title: String,
    pub body: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub session: Option<String>,
    pub ts: u64,
}

impl Notification {
    pub fn new(kind: &str, title: &str, body: &str, session: Option<&str>) -> Self {
        Notification {
            v: 1,
            kind: kind.into(),
            title: title.into(),
            body: body.into(),
            session: session.map(str::to_string),
            ts: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        }
    }

    /// Compact JSON, padded to a `PAD_BLOCK` multiple. Long bodies are
    /// shortened (on a char boundary) to keep under `MAX_PLAINTEXT`.
    pub fn plaintext(&self) -> Vec<u8> {
        let mut n = self.clone();
        loop {
            let mut out = serde_json::to_vec(&n).unwrap_or_default();
            if out.len() <= MAX_PLAINTEXT || n.body.is_empty() {
                let padded = out.len().div_ceil(PAD_BLOCK) * PAD_BLOCK;
                out.resize(padded.max(PAD_BLOCK), b' ');
                return out;
            }
            let keep = n.body.chars().count().saturating_sub(64);
            n.body = n.body.chars().take(keep).collect::<String>();
            if keep > 0 {
                n.body.push('…');
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    High,
    Normal,
}

impl Priority {
    fn as_str(self) -> &'static str {
        match self {
            Priority::High => "high",
            Priority::Normal => "normal",
        }
    }
}

/// Map a Claude Code hook event (the `/api/hook-event` body the daemon
/// broadcasts) to a notification, its priority and a collapse key.
/// Events that don't need the user return `None`.
pub fn from_hook_event(ev: &Value) -> Option<(Notification, Priority, String)> {
    let name = ev.get("hook_event_name")?.as_str()?;
    let session = ev
        .get("session_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let project = ev
        .get("cwd")
        .and_then(Value::as_str)
        .and_then(|c| Path::new(c).file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("Claude");
    let message = ev
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let pick = |fallback: &str| {
        if message.is_empty() {
            fallback.to_string()
        } else {
            message.to_string()
        }
    };
    let (kind, title, body, prio) = match name {
        "Notification" => match ev.get("notification_type").and_then(Value::as_str) {
            Some("permission_prompt") => (
                "needs_permission",
                format!("{project} needs permission"),
                pick("Claude needs your permission"),
                Priority::High,
            ),
            Some("idle_prompt") | None => (
                "needs_input",
                format!("{project} is waiting"),
                pick("Claude is waiting for your input"),
                Priority::High,
            ),
            Some(_) => return None,
        },
        "Stop" => (
            "finished",
            format!("{project} finished"),
            pick("Claude finished working"),
            Priority::Normal,
        ),
        _ => return None,
    };
    let collapse = format!("{}|{}", session.unwrap_or(project), kind);
    Some((
        Notification::new(kind, &title, &body, session),
        prio,
        collapse,
    ))
}

// ── devices ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApnsEnv {
    Sandbox,
    Production,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Device {
    /// APNs device token, lowercase hex.
    pub device_token: String,
    /// X25519 public key, standard base64 of 32 raw bytes.
    pub public_key: String,
    pub env: ApnsEnv,
    /// Relay-issued send credential (delivered to the phone by push).
    pub credential: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub name: Option<String>,
    pub registered_at: u64,
}

impl Device {
    pub fn public_key_bytes(&self) -> Result<[u8; 32], PushError> {
        let b = B64
            .decode(&self.public_key)
            .map_err(|_| PushError::BadKey)?;
        if b.len() != 32 {
            return Err(PushError::BadKey);
        }
        Ok(to32(&b))
    }

    /// Opaque, stable per (device, key): the relay and Apple see a hash
    /// keyed by the device's public key, not session names.
    pub fn collapse_id(&self, key: &str) -> String {
        let mut h = Sha256::new();
        h.update(b"baton collapse v1\0");
        h.update(self.public_key.as_bytes());
        h.update([0]);
        h.update(key.as_bytes());
        B64URL.encode(&h.finalize()[..16])
    }
}

/// Validate a `POST /api/push/register` body.
pub fn parse_registration(v: &Value) -> Result<Device, String> {
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::trim);
    let token = s("device_token")
        .ok_or("device_token required")?
        .to_ascii_lowercase();
    if !(64..=200).contains(&token.len()) || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("device_token must be hex".into());
    }
    let pk = s("public_key").ok_or("public_key required")?;
    match B64.decode(pk) {
        Ok(b) if b.len() == 32 => {}
        _ => return Err("public_key must be base64 of 32 bytes (X25519)".into()),
    }
    let env = match s("env").unwrap_or("production") {
        "sandbox" => ApnsEnv::Sandbox,
        "production" => ApnsEnv::Production,
        _ => return Err("env must be sandbox or production".into()),
    };
    let credential = s("credential").ok_or("credential required")?;
    if credential.len() != 43
        || !credential
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err("credential malformed".into());
    }
    let name = s("name")
        .filter(|n| !n.is_empty())
        .map(|n| n.chars().take(64).collect());
    Ok(Device {
        device_token: token,
        public_key: pk.to_string(),
        env,
        credential: credential.to_string(),
        name,
        registered_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    })
}

#[derive(Serialize, Deserialize, Default)]
struct StoreFile {
    version: u32,
    devices: Vec<Device>,
}

/// Registered devices, a JSON file written atomically with mode 0600 (it
/// holds relay credentials).
pub struct DeviceStore {
    path: PathBuf,
    lock: Mutex<()>,
}

impl DeviceStore {
    pub fn new(path: PathBuf) -> Self {
        DeviceStore {
            path,
            lock: Mutex::new(()),
        }
    }

    pub fn default_path(home: &Path) -> PathBuf {
        match std::env::var_os("BATON_PUSH_DEVICES_FILE") {
            Some(p) if !p.is_empty() => PathBuf::from(p),
            _ => home.join(".config/baton/push-devices.json"),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn read(&self) -> StoreFile {
        std::fs::read(&self.path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or(StoreFile {
                version: 1,
                devices: vec![],
            })
    }

    fn write(&self, f: &StoreFile) -> Result<(), PushError> {
        let io = |e: std::io::Error| PushError::Io(e.to_string());
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(io)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        let data = serde_json::to_vec_pretty(f).map_err(|e| PushError::Io(e.to_string()))?;
        {
            use std::io::Write;
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create(true).truncate(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
            let mut file = opts.open(&tmp).map_err(io)?;
            file.write_all(&data).map_err(io)?;
            file.sync_all().map_err(io)?;
        }
        std::fs::rename(&tmp, &self.path).map_err(io)
    }

    pub fn list(&self) -> Vec<Device> {
        let _g = self.lock.lock().unwrap_or_else(|p| p.into_inner());
        self.read().devices
    }

    /// Insert or replace (by token); returns the device count.
    pub fn upsert(&self, d: Device) -> Result<usize, PushError> {
        let _g = self.lock.lock().unwrap_or_else(|p| p.into_inner());
        let mut f = self.read();
        f.version = 1;
        f.devices.retain(|x| x.device_token != d.device_token);
        f.devices.push(d);
        self.write(&f)?;
        Ok(f.devices.len())
    }

    /// Returns whether a device was removed.
    pub fn remove(&self, token: &str) -> Result<bool, PushError> {
        let _g = self.lock.lock().unwrap_or_else(|p| p.into_inner());
        let mut f = self.read();
        let before = f.devices.len();
        let token = token.to_ascii_lowercase();
        f.devices.retain(|x| x.device_token != token);
        if f.devices.len() == before {
            return Ok(false);
        }
        self.write(&f)?;
        Ok(true)
    }
}

// ── relay client ───────────────────────────────────────────────────────────

#[derive(Debug, PartialEq, Eq)]
pub enum SendOutcome {
    Delivered,
    /// APNs no longer knows the token: drop the device.
    Unregistered,
    /// Relay or APNs throttled; `retry_after` in seconds when known.
    RateLimited(Option<u64>),
    /// The relay rejected the credential: the phone must re-register.
    Forbidden,
    Failed(u16, String),
}

type HttpsClient = Client<hyper_rustls::HttpsConnector<HttpConnector>, Full<Bytes>>;

pub struct RelayClient {
    base: String,
    client: HttpsClient,
}

impl RelayClient {
    /// `base` like `https://relay.example.com` (http:// allowed for a
    /// local `wrangler dev`).
    pub fn new(base: &str) -> Self {
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_or_http()
            .enable_http1()
            .build();
        let client = Client::builder(TokioExecutor::new())
            .pool_idle_timeout(Duration::from_secs(60))
            .build(https);
        RelayClient {
            base: base.trim_end_matches('/').to_string(),
            client,
        }
    }

    /// The relay URL as resolved by the config (`--relay` >
    /// `BATON_RELAY_URL` > `relay` in the config file). Empty or missing =
    /// push disabled.
    pub fn from_env(config_value: Option<String>) -> Option<Self> {
        let url = config_value
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())?;
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            return None;
        }
        Some(Self::new(&url))
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    /// The `/v1/send` body for one device.
    pub fn send_body(
        device: &Device,
        sealed: &[u8],
        collapse_id: Option<&str>,
        priority: Priority,
    ) -> Value {
        let mut v = json!({
            "device_token": device.device_token,
            "env": device.env,
            "credential": device.credential,
            "ciphertext": B64.encode(sealed),
            "priority": priority.as_str(),
        });
        if let Some(c) = collapse_id {
            v["collapse_id"] = json!(c);
        }
        v
    }

    pub async fn send(
        &self,
        device: &Device,
        n: &Notification,
        collapse_key: Option<&str>,
        priority: Priority,
    ) -> Result<SendOutcome, PushError> {
        let sealed = seal(&device.public_key_bytes()?, &n.plaintext())?;
        let collapse = collapse_key.map(|k| device.collapse_id(k));
        let body = Self::send_body(device, &sealed, collapse.as_deref(), priority);
        let req = hyper::Request::post(format!("{}/v1/send", self.base))
            .header("content-type", "application/json")
            .header("user-agent", concat!("baton/", env!("CARGO_PKG_VERSION")))
            .body(Full::new(Bytes::from(body.to_string())))
            .map_err(|e| PushError::Http(e.to_string()))?;
        let resp = tokio::time::timeout(Duration::from_secs(15), self.client.request(req))
            .await
            .map_err(|_| PushError::Http("timeout".into()))?
            .map_err(|e| PushError::Http(e.to_string()))?;
        let status = resp.status().as_u16();
        let retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok());
        let bytes = resp
            .into_body()
            .collect()
            .await
            .map(|b| b.to_bytes())
            .unwrap_or_default();
        let err = serde_json::from_slice::<Value>(&bytes)
            .ok()
            .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_string))
            .unwrap_or_default();
        Ok(match status {
            200 => SendOutcome::Delivered,
            410 => SendOutcome::Unregistered,
            429 => SendOutcome::RateLimited(retry_after),
            401 | 403 => SendOutcome::Forbidden,
            s => SendOutcome::Failed(s, err),
        })
    }
}

// ── daemon-facing API ──────────────────────────────────────────────────────

/// What `State` holds: the device store and (when configured) the relay.
pub struct Push {
    pub store: DeviceStore,
    pub relay: Option<RelayClient>,
}

impl Push {
    pub fn new(store: DeviceStore, relay: Option<RelayClient>) -> Self {
        Push { store, relay }
    }

    /// Store from `BATON_PUSH_DEVICES_FILE`, relay from `BATON_RELAY_URL`
    /// or the `relay=` config value.
    pub fn from_env(home: &Path, relay_config: Option<String>) -> Self {
        Push::new(
            DeviceStore::new(DeviceStore::default_path(home)),
            RelayClient::from_env(relay_config),
        )
    }

    /// `POST /api/push/register` → (status, JSON body).
    pub fn register(&self, body: &[u8]) -> (u16, Value) {
        let v: Value = match serde_json::from_slice(body) {
            Ok(v @ Value::Object(_)) => v,
            _ => return (400, json!({"error": "JSON object required"})),
        };
        let d = match parse_registration(&v) {
            Ok(d) => d,
            Err(e) => return (400, json!({"error": e})),
        };
        match self.store.upsert(d) {
            Ok(n) => (
                200,
                json!({"ok": true, "devices": n, "relay": self.relay.as_ref().map(|r| r.base())}),
            ),
            Err(e) => (500, json!({"error": e.to_string()})),
        }
    }

    /// `POST /api/push/unregister {device_token}` → (status, JSON body).
    pub fn unregister(&self, body: &[u8]) -> (u16, Value) {
        let token = serde_json::from_slice::<Value>(body).ok().and_then(|v| {
            v.get("device_token")
                .and_then(Value::as_str)
                .map(str::to_string)
        });
        let Some(token) = token else {
            return (400, json!({"error": "device_token required"}));
        };
        match self.store.remove(&token) {
            Ok(removed) => (200, json!({"ok": true, "removed": removed})),
            Err(e) => (500, json!({"error": e.to_string()})),
        }
    }

    /// Send to every registered device. Devices APNs reports as gone are
    /// dropped from the store. Returns how many were delivered.
    pub async fn notify_all(
        &self,
        n: &Notification,
        collapse_key: Option<&str>,
        priority: Priority,
    ) -> usize {
        let Some(relay) = &self.relay else {
            return 0;
        };
        let mut delivered = 0;
        for d in self.store.list() {
            match relay.send(&d, n, collapse_key, priority).await {
                Ok(SendOutcome::Delivered) => delivered += 1,
                Ok(SendOutcome::Unregistered) => {
                    let _ = self.store.remove(&d.device_token);
                }
                Ok(other) => eprintln!("push: {}…: {other:?}", &d.device_token[..8]),
                Err(e) => eprintln!("push: {}…: {e}", &d.device_token[..8]),
            }
        }
        delivered
    }

    /// Forward hook events from the daemon's event bus as notifications.
    /// Spawn once at startup with `state.events.subscribe()`.
    pub async fn run(self: std::sync::Arc<Self>, mut rx: tokio::sync::broadcast::Receiver<Value>) {
        use tokio::sync::broadcast::error::RecvError;
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    if let Some((n, prio, key)) = from_hook_event(&ev) {
                        let me = self.clone();
                        tokio::spawn(async move {
                            me.notify_all(&n, Some(&key), prio).await;
                        });
                    }
                }
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => return,
            }
        }
    }
}

// ── tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Test vectors shared with apple/PushCrypto (Swift/CryptoKit).
    const VECTORS: &str = include_str!("../testdata/push_vectors.json");

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    fn hexs(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    /// Yields fixed bytes: HPKE draws exactly `ikmE` (32 bytes) for the
    /// ephemeral key, so this reproduces a vector's `enc`.
    struct FixedRng(Vec<u8>, usize);

    impl RngCore for FixedRng {
        fn next_u32(&mut self) -> u32 {
            let mut b = [0u8; 4];
            self.fill_bytes(&mut b);
            u32::from_le_bytes(b)
        }
        fn next_u64(&mut self) -> u64 {
            let mut b = [0u8; 8];
            self.fill_bytes(&mut b);
            u64::from_le_bytes(b)
        }
        fn fill_bytes(&mut self, dst: &mut [u8]) {
            for b in dst {
                *b = self.0[self.1 % self.0.len()];
                self.1 += 1;
            }
        }
    }
    impl CryptoRng for FixedRng {}

    fn vectors() -> Value {
        serde_json::from_str(VECTORS).unwrap()
    }

    fn field(v: &Value, k: &str) -> Vec<u8> {
        hex(v[k].as_str().unwrap_or_else(|| panic!("missing {k}")))
    }

    #[test]
    fn push_rfc9180_known_answer() {
        // RFC 9180 A.2.1: DHKEM(X25519)/HKDF-SHA256/ChaCha20-Poly1305, base.
        let v = &vectors()["rfc9180_a2_1"];
        let (sk, pk) = derive_keypair(&field(v, "ikmR"));
        assert_eq!(hexs(&sk), v["skRm"].as_str().unwrap());
        assert_eq!(hexs(&pk), v["pkRm"].as_str().unwrap());
        let sealed = seal_with_rng(
            &pk,
            &field(v, "pt"),
            &field(v, "info"),
            &field(v, "aad"),
            &mut FixedRng(field(v, "ikmE"), 0),
        )
        .unwrap();
        assert_eq!(hexs(&sealed[..ENC_LEN]), v["enc"].as_str().unwrap());
        assert_eq!(hexs(&sealed[ENC_LEN..]), v["ct"].as_str().unwrap());
        let pt = open(&sk, &sealed, &field(v, "info"), &field(v, "aad")).unwrap();
        assert_eq!(pt, field(v, "pt"));
    }

    #[test]
    fn push_baton_vector_matches_swift() {
        // The same vector apple/PushCrypto decrypts with CryptoKit.
        let v = &vectors()["baton_v1"];
        assert_eq!(v["info"].as_str().unwrap().as_bytes(), HPKE_INFO);
        let (sk, pk) = derive_keypair(&field(v, "ikmR"));
        assert_eq!(hexs(&sk), v["skR"].as_str().unwrap());
        assert_eq!(B64.encode(pk), v["public_key_b64"].as_str().unwrap());

        let n: Notification = serde_json::from_value(v["notification"].clone()).unwrap();
        let pt = n.plaintext();
        assert_eq!(hexs(&pt), v["plaintext_hex"].as_str().unwrap());

        let sealed = seal_with_rng(
            &pk,
            &pt,
            HPKE_INFO,
            HPKE_AAD,
            &mut FixedRng(field(v, "ikmE"), 0),
        )
        .unwrap();
        assert_eq!(B64.encode(&sealed), v["sealed_b64"].as_str().unwrap());
        assert_eq!(open(&sk, &sealed, HPKE_INFO, HPKE_AAD).unwrap(), pt);
    }

    #[test]
    fn push_seal_roundtrip_and_tamper() {
        let (sk, pk) = generate_keypair();
        let n = Notification::new("finished", "baton finished", "All tests pass", Some("abc"));
        let pt = n.plaintext();
        let a = seal(&pk, &pt).unwrap();
        let b = seal(&pk, &pt).unwrap();
        assert_ne!(a, b, "fresh ephemeral key per message");
        assert_eq!(open(&sk, &a, HPKE_INFO, HPKE_AAD).unwrap(), pt);
        let back: Notification =
            serde_json::from_slice(&open(&sk, &b, HPKE_INFO, HPKE_AAD).unwrap()).unwrap();
        assert_eq!(back, n);

        let mut bad = a.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert_eq!(open(&sk, &bad, HPKE_INFO, HPKE_AAD), Err(PushError::Crypto));
        assert_eq!(
            open(&sk, &a, b"baton push v2", HPKE_AAD),
            Err(PushError::Crypto)
        );
        let (other, _) = generate_keypair();
        assert_eq!(
            open(&other, &a, HPKE_INFO, HPKE_AAD),
            Err(PushError::Crypto)
        );
        assert_eq!(
            open(&sk, &a[..40], HPKE_INFO, HPKE_AAD),
            Err(PushError::Crypto)
        );
    }

    #[test]
    fn push_plaintext_is_padded_and_bounded() {
        let n = Notification::new("needs_input", "t", "b", None);
        let pt = n.plaintext();
        assert_eq!(pt.len() % PAD_BLOCK, 0);
        assert!(serde_json::from_slice::<Notification>(&pt).is_ok());

        let long = Notification::new("finished", "t", &"é".repeat(5000), Some("s"));
        let pt = long.plaintext();
        assert!(pt.len() <= MAX_PLAINTEXT + PAD_BLOCK);
        let back: Notification = serde_json::from_slice(&pt).unwrap();
        assert!(back.body.ends_with('…'));
        // Sealed size fits the relay's 2048-byte ciphertext limit.
        let (_, pk) = generate_keypair();
        assert!(seal(&pk, &pt).unwrap().len() <= 2048);
    }

    #[test]
    fn push_hook_events_map_to_notifications() {
        let perm = json!({"hook_event_name": "Notification", "notification_type": "permission_prompt",
            "message": "Claude needs your permission to use Bash", "session_id": "s1", "cwd": "/Users/a/GitHub/baton"});
        let (n, p, key) = from_hook_event(&perm).unwrap();
        assert_eq!(
            (n.kind.as_str(), n.title.as_str(), p),
            ("needs_permission", "baton needs permission", Priority::High)
        );
        assert_eq!(n.body, "Claude needs your permission to use Bash");
        assert_eq!(n.session.as_deref(), Some("s1"));
        assert_eq!(key, "s1|needs_permission");

        let stop = json!({"hook_event_name": "Stop", "session_id": "s2", "cwd": "/tmp/proj"});
        let (n, p, _) = from_hook_event(&stop).unwrap();
        assert_eq!(
            (n.kind.as_str(), n.body.as_str(), p),
            ("finished", "Claude finished working", Priority::Normal)
        );

        let idle = json!({"hook_event_name": "Notification", "notification_type": "idle_prompt"});
        assert_eq!(from_hook_event(&idle).unwrap().0.title, "Claude is waiting");

        for ignored in [
            json!({"hook_event_name": "SessionStart"}),
            json!({"hook_event_name": "Notification", "notification_type": "auth_success"}),
            json!({"type": "resume-ready"}),
        ] {
            assert!(from_hook_event(&ignored).is_none());
        }
    }

    fn reg_body(token: &str) -> Value {
        let (_, pk) = generate_keypair();
        json!({"device_token": token, "public_key": B64.encode(pk), "env": "sandbox",
               "credential": "A".repeat(43), "name": "iPhone"})
    }

    #[test]
    fn push_registration_validation() {
        let t = "AB".repeat(32);
        let d = parse_registration(&reg_body(&t)).unwrap();
        assert_eq!(d.device_token, t.to_lowercase());
        assert_eq!(d.env, ApnsEnv::Sandbox);
        assert_eq!(d.name.as_deref(), Some("iPhone"));

        let bad = |patch: Value| {
            let mut b = reg_body(&t);
            for (k, v) in patch.as_object().unwrap() {
                b[k] = v.clone();
            }
            parse_registration(&b).unwrap_err()
        };
        bad(json!({"device_token": "xyz"}));
        bad(json!({"device_token": "ab"}));
        bad(json!({"public_key": B64.encode([1u8; 31])}));
        bad(json!({"public_key": "***"}));
        bad(json!({"env": "staging"}));
        bad(json!({"credential": "short"}));
        bad(json!({"credential": format!("{}+", "A".repeat(42))}));
        let mut no_env = reg_body(&t);
        no_env.as_object_mut().unwrap().remove("env");
        assert_eq!(
            parse_registration(&no_env).unwrap().env,
            ApnsEnv::Production
        );
    }

    fn temp_store(tag: &str) -> DeviceStore {
        let dir = std::env::temp_dir().join(format!("baton-push-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        DeviceStore::new(dir.join("sub/push-devices.json"))
    }

    #[test]
    fn push_device_store_roundtrip() {
        let store = temp_store("store");
        assert!(store.list().is_empty());
        let a = parse_registration(&reg_body(&"a1".repeat(32))).unwrap();
        let b = parse_registration(&reg_body(&"b2".repeat(32))).unwrap();
        assert_eq!(store.upsert(a.clone()).unwrap(), 1);
        assert_eq!(store.upsert(b.clone()).unwrap(), 2);
        let mut a2 = a.clone();
        a2.name = Some("renamed".into());
        assert_eq!(store.upsert(a2.clone()).unwrap(), 2, "same token replaces");
        let list = store.list();
        assert!(list.contains(&a2) && list.contains(&b));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(store.path())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert!(store.remove(&b.device_token.to_uppercase()).unwrap());
        assert!(!store.remove(&b.device_token).unwrap());
        assert_eq!(store.list(), vec![a2]);
        let raw: Value = serde_json::from_slice(&std::fs::read(store.path()).unwrap()).unwrap();
        assert_eq!(raw["version"], 1);
    }

    #[test]
    fn push_register_endpoint_handler() {
        let push = Push::new(temp_store("handler"), None);
        let (s, v) = push.register(reg_body(&"c3".repeat(32)).to_string().as_bytes());
        assert_eq!((s, &v["ok"], &v["devices"]), (200, &json!(true), &json!(1)));
        assert_eq!(v["relay"], Value::Null);
        assert_eq!(push.register(b"[1]").0, 400);
        assert_eq!(push.register(b"{\"device_token\":\"zz\"}").0, 400);
        let (s, v) =
            push.unregister(format!("{{\"device_token\":\"{}\"}}", "c3".repeat(32)).as_bytes());
        assert_eq!((s, &v["removed"]), (200, &json!(true)));
        assert_eq!(push.unregister(b"{}").0, 400);
    }

    #[test]
    fn push_collapse_id_is_opaque_and_stable() {
        let d = parse_registration(&reg_body(&"d4".repeat(32))).unwrap();
        let a = d.collapse_id("claude-myproj|finished");
        assert_eq!(a, d.collapse_id("claude-myproj|finished"));
        assert_ne!(a, d.collapse_id("claude-other|finished"));
        assert!(!a.contains("myproj"));
        assert!(a.len() <= 64);
        let other = parse_registration(&reg_body(&"d4".repeat(32))).unwrap();
        assert_ne!(
            a,
            other.collapse_id("claude-myproj|finished"),
            "keyed per device"
        );
    }

    #[test]
    fn push_relay_url_config() {
        assert!(RelayClient::from_env(None).is_none());
        assert!(RelayClient::from_env(Some("  ".into())).is_none());
        assert!(RelayClient::from_env(Some("ftp://x".into())).is_none());
        let c = RelayClient::from_env(Some("https://relay.example.com/".into())).unwrap();
        assert_eq!(c.base(), "https://relay.example.com");
    }

    /// A one-shot local HTTP server standing in for the relay: records the
    /// request and answers with `status` + `body`.
    async fn fake_relay(
        responses: Vec<(u16, &'static str)>,
    ) -> (
        String,
        tokio::sync::mpsc::UnboundedReceiver<(String, Value)>,
    ) {
        use hyper::server::conn::http1;
        use hyper::service::service_fn;
        use hyper_util::rt::TokioIo;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let responses = std::sync::Arc::new(Mutex::new(responses.into_iter()));
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let tx = tx.clone();
                let responses = responses.clone();
                tokio::spawn(async move {
                    let svc = service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                        let tx = tx.clone();
                        let responses = responses.clone();
                        async move {
                            let path = req.uri().path().to_string();
                            let body = req.into_body().collect().await.unwrap().to_bytes();
                            tx.send((path, serde_json::from_slice(&body).unwrap()))
                                .unwrap();
                            let (status, text) = responses
                                .lock()
                                .unwrap()
                                .next()
                                .unwrap_or((200, "{\"ok\":true}"));
                            let mut resp = hyper::Response::new(Full::new(Bytes::from(text)));
                            *resp.status_mut() = hyper::StatusCode::from_u16(status).unwrap();
                            if status == 429 {
                                resp.headers_mut()
                                    .insert("retry-after", "42".parse().unwrap());
                            }
                            Ok::<_, std::convert::Infallible>(resp)
                        }
                    });
                    let _ = http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), svc)
                        .await;
                });
            }
        });
        (format!("http://{addr}"), rx)
    }

    #[tokio::test]
    async fn push_client_sends_sealed_payload_to_relay() {
        let (base, mut rx) = fake_relay(vec![(200, "{\"ok\":true}")]).await;
        let relay = RelayClient::new(&base);
        let (sk, pk) = generate_keypair();
        let mut d = parse_registration(&reg_body(&"e5".repeat(32))).unwrap();
        d.public_key = B64.encode(pk);
        let n = Notification::new(
            "needs_input",
            "proj is waiting",
            "secret text",
            Some("session-marker"),
        );
        let out = relay
            .send(&d, &n, Some("session-marker|needs_input"), Priority::High)
            .await
            .unwrap();
        assert_eq!(out, SendOutcome::Delivered);

        let (path, body) = rx.recv().await.unwrap();
        assert_eq!(path, "/v1/send");
        assert_eq!(body["device_token"], d.device_token);
        assert_eq!(body["env"], "sandbox");
        assert_eq!(body["credential"], d.credential);
        assert_eq!(body["priority"], "high");
        assert_eq!(
            body["collapse_id"],
            d.collapse_id("session-marker|needs_input")
        );
        let text = body.to_string();
        assert!(
            !text.contains("secret text") && !text.contains("session-marker"),
            "no plaintext on the wire"
        );
        let sealed = B64.decode(body["ciphertext"].as_str().unwrap()).unwrap();
        let back: Notification =
            serde_json::from_slice(&open(&sk, &sealed, HPKE_INFO, HPKE_AAD).unwrap()).unwrap();
        assert_eq!(back, n);
    }

    #[tokio::test]
    async fn push_notify_all_prunes_unregistered_devices() {
        let (base, mut rx) = fake_relay(vec![
            (200, "{\"ok\":true}"),
            (410, "{\"ok\":false,\"error\":\"unregistered\"}"),
            (429, "{\"ok\":false,\"error\":\"rate_limited\"}"),
            (403, "{\"ok\":false,\"error\":\"bad_credential\"}"),
        ])
        .await;
        let store = temp_store("notify");
        let tokens = ["f1", "f2", "f3", "f4"].map(|t| t.repeat(32));
        for t in &tokens {
            store
                .upsert(parse_registration(&reg_body(t)).unwrap())
                .unwrap();
        }
        let push = Push::new(store, Some(RelayClient::new(&base)));
        let n = Notification::new("finished", "t", "b", None);
        assert_eq!(push.notify_all(&n, None, Priority::Normal).await, 1);
        let mut seen = vec![];
        while let Ok((_, b)) = rx.try_recv() {
            assert_eq!(b["priority"], "normal");
            assert!(b.get("collapse_id").is_none());
            seen.push(b["device_token"].as_str().unwrap().to_string());
        }
        assert_eq!(seen, tokens.to_vec());
        let left: Vec<String> = push
            .store
            .list()
            .into_iter()
            .map(|d| d.device_token)
            .collect();
        assert_eq!(
            left,
            vec![tokens[0].clone(), tokens[2].clone(), tokens[3].clone()]
        );

        // Individual outcome mapping.
        let (base, _rx) = fake_relay(vec![
            (429, "{}"),
            (403, "{}"),
            (502, "{\"error\":\"apns_error\"}"),
        ])
        .await;
        let relay = RelayClient::new(&base);
        let d = parse_registration(&reg_body(&tokens[0])).unwrap();
        assert_eq!(
            relay.send(&d, &n, None, Priority::High).await.unwrap(),
            SendOutcome::RateLimited(Some(42))
        );
        assert_eq!(
            relay.send(&d, &n, None, Priority::High).await.unwrap(),
            SendOutcome::Forbidden
        );
        assert_eq!(
            relay.send(&d, &n, None, Priority::High).await.unwrap(),
            SendOutcome::Failed(502, "apns_error".into())
        );
    }

    #[tokio::test]
    async fn push_without_relay_is_a_noop() {
        let push = Push::new(temp_store("norelay"), None);
        push.store
            .upsert(parse_registration(&reg_body(&"a9".repeat(32))).unwrap())
            .unwrap();
        assert_eq!(
            push.notify_all(
                &Notification::new("finished", "t", "b", None),
                None,
                Priority::High
            )
            .await,
            0
        );
    }
}
