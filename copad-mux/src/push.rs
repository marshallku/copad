//! Agent notifications pushed to phones over APNs (decisions #175, #177).
//!
//! The copad iOS app registers its device token through the L0 command channel
//! (`push-register`); this server keeps the registrations on disk and, for every notification it
//! raises, hands one push per device to a worker thread. Nothing here runs on the server loop: the
//! loop only enqueues, and learns about dead tokens by polling.
//!
//! Every push is end-to-end encrypted. The app sends a 32-byte key per host entry with its
//! registration; the title, body and pane are sealed with AES-256-GCM and only the app's
//! notification service extension can open them. What leaves this machine is a placeholder alert
//! plus the ciphertext, so neither the relay nor Apple sees what an agent said.
//!
//! Two ways out (`[push]` in mux.toml): the push relay (default — it holds the app's APNs key, so
//! no Apple developer account is needed here), or the team's own APNs key for a self-built app (a
//! `.p8` PKCS#8 P-256 key; the JWT is signed with `ring` and reused for 50 minutes).

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use serde::{Deserialize, Serialize};

/// Registrations kept; the oldest go first.
const MAX_REGS: usize = 64;
/// Pushes waiting for the worker; past it the oldest is dropped (a later one supersedes it).
const QUEUE_CAP: usize = 64;
const TITLE_MAX: usize = 160;
const BODY_MAX: usize = 1024;
/// The sealed plaintext's budget: its base64 envelope must fit the relay's 3000 bytes, and the
/// whole payload Apple's 4 KB.
const SEALED_PLAIN_MAX: usize = 2200;
const JWT_TTL: Duration = Duration::from_secs(50 * 60);
/// The relay serving the copad app (AIDE's `aide push-relay`; AIDE ADR-0023).
pub const DEFAULT_RELAY: &str = "https://aide-push.marshallku.dev";
/// What a push shows when it can't be opened; the relay and Apple see only this.
pub const PLACEHOLDER_TITLE: &str = "copad";
pub const PLACEHOLDER_BODY: &str = "Agent notification";
/// The envelope version the app checks for (`e2e` in the `push-register` reply).
pub const E2E_VERSION: u32 = 1;
const AAD_PREFIX: &[u8] = b"comux-push-v1\0";

/// The team's own APNs key (`[push] apns_key`, `apns_key_id`, `apns_team_id`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushConfig {
    pub key_file: PathBuf,
    pub key_id: String,
    pub team_id: String,
}

/// How pushes leave this server (`[push]` in mux.toml).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushMode {
    Off,
    /// Through the push relay at this base URL.
    Relay(String),
    /// Straight to APNs with the team's key.
    Direct(PushConfig),
}

impl Default for PushMode {
    fn default() -> Self {
        Self::Relay(DEFAULT_RELAY.to_string())
    }
}

impl PushMode {
    pub fn describe(&self) -> String {
        match self {
            Self::Off => "off".into(),
            Self::Relay(url) => format!("relay {url}"),
            Self::Direct(c) => format!("APNs key {}", c.key_id),
        }
    }
}

/// A relay URL: https, or http to this machine (tests, a local relay).
pub fn check_relay_url(url: &str) -> Result<(), String> {
    let Ok(u) = reqwest::Url::parse(url) else {
        return Err(format!("push relay '{url}' is not a URL"));
    };
    let local = matches!(
        u.host_str(),
        Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
    );
    if u.query().is_some() || u.fragment().is_some() || u.host_str().is_none() {
        return Err(format!("push relay '{url}' is not a base URL"));
    }
    match u.scheme() {
        "https" => Ok(()),
        "http" if local => Ok(()),
        _ => Err(format!("push relay '{url}' must use https")),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApnsEnv {
    Sandbox,
    Production,
}

/// One app install on one saved host entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reg {
    pub token: String,
    pub env: ApnsEnv,
    /// The app's bundle id (`apns-topic`).
    pub topic: String,
    /// The app's id for the host entry it registered through: echoed in the payload so a tap
    /// opens THAT entry.
    pub host: String,
    #[serde(default)]
    pub device: String,
    /// The app's key for this host entry (base64, 32 bytes): every push to it is sealed with
    /// it. Registrations stored before encryption have none and are never sent to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Monotonic; a dead-token report only removes registrations at or before it, so one
    /// refreshed meanwhile survives.
    pub generation: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Stored {
    regs: Vec<Reg>,
    next: u64,
}

/// The registrations, persisted next to the session state file.
pub struct Registry {
    path: PathBuf,
    st: Stored,
}

/// A `push-register` request.
pub struct NewReg<'a> {
    pub token: &'a str,
    pub env: ApnsEnv,
    pub topic: &'a str,
    pub host: &'a str,
    pub device: &'a str,
    pub key: &'a str,
}

/// A device token: hex, as the app prints `deviceToken`.
fn valid_token(t: &str) -> bool {
    (32..=200).contains(&t.len()) && t.bytes().all(|b| b.is_ascii_hexdigit())
}

fn valid_word(s: &str, max: usize) -> bool {
    !s.is_empty()
        && s.len() <= max
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._".contains(&b))
}

impl Registry {
    /// `comux-push.json` beside the state file.
    pub fn default_path() -> PathBuf {
        crate::persist::state_path().with_file_name("comux-push.json")
    }

    pub fn load(path: PathBuf) -> Self {
        let st = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Stored>(&b).ok())
            .unwrap_or_default();
        Self { path, st }
    }

    pub fn len(&self) -> usize {
        self.st.regs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.st.regs.is_empty()
    }

    /// Add or refresh `(token, host)`. Refused input is reported, never stored.
    pub fn register(&mut self, r: NewReg<'_>) -> Result<(), String> {
        let NewReg {
            token,
            env,
            topic,
            host,
            device,
            key,
        } = r;
        let token = token.to_ascii_lowercase();
        if !valid_token(&token) {
            return Err("not an APNs device token".into());
        }
        // The host id doubles as the relay's `kid` (at most 64 of the same characters).
        if !valid_word(topic, 200) || !valid_word(host, 64) {
            return Err("bad topic or host id".into());
        }
        if parse_key(key).is_none() {
            return Err(
                "this copad app sends no encryption key: update the app to get pushes".into(),
            );
        }
        self.st.next += 1;
        let generation = self.st.next;
        self.st
            .regs
            .retain(|r| !(r.token == token && r.host == host));
        self.st.regs.push(Reg {
            token,
            env,
            topic: topic.to_string(),
            host: host.to_string(),
            device: device.chars().take(80).collect(),
            key: Some(key.to_string()),
            generation,
        });
        while self.st.regs.len() > MAX_REGS {
            self.st.regs.remove(0);
        }
        self.save()
    }

    /// Forget `token` (for one host entry, or all of them).
    pub fn unregister(&mut self, token: &str, host: Option<&str>) -> Result<bool, String> {
        let token = token.to_ascii_lowercase();
        let before = self.st.regs.len();
        self.st
            .regs
            .retain(|r| !(r.token == token && host.is_none_or(|h| r.host == h)));
        let changed = self.st.regs.len() != before;
        if changed {
            self.save()?;
        }
        Ok(changed)
    }

    /// One registration per device token: the most recent one (the host entry the user last
    /// connected through), so a device never gets the same notification twice. Registrations
    /// without a key (stored before pushes were encrypted) are skipped: nothing goes out in the
    /// clear.
    pub fn targets(&self) -> Vec<Reg> {
        let mut out: Vec<Reg> = Vec::new();
        for r in self.st.regs.iter().filter(|r| r.key.is_some()) {
            match out.iter_mut().find(|x| x.token == r.token) {
                Some(x) if x.generation < r.generation => *x = r.clone(),
                Some(_) => {}
                None => out.push(r.clone()),
            }
        }
        out
    }

    /// APNs said `token` is dead as of `generation`: drop it, unless re-registered since.
    pub fn drop_dead(&mut self, token: &str, generation: u64) {
        let before = self.st.regs.len();
        self.st
            .regs
            .retain(|r| !(r.token == token && r.generation <= generation));
        if self.st.regs.len() != before {
            let _ = self.save();
        }
    }

    /// temp + fsync + rename, mode 0600 (device tokens are not secrets, but they are the
    /// owner's).
    fn save(&self) -> Result<(), String> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let dir = self.path.parent().unwrap_or(Path::new("."));
        let _ = std::fs::create_dir_all(dir);
        let tmp = self.path.with_extension("json.tmp");
        let body = serde_json::to_vec_pretty(&self.st).map_err(|e| e.to_string())?;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| format!("{}: {e}", tmp.display()))?;
        f.write_all(&body).map_err(|e| e.to_string())?;
        f.sync_all().map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &self.path).map_err(|e| e.to_string())
    }
}

/// Cut `s` to at most `max` bytes on a char boundary, marking the cut.
fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max.saturating_sub('…'.len_utf8());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

fn parse_key(b64: &str) -> Option<[u8; 32]> {
    base64::engine::general_purpose::STANDARD
        .decode(b64)
        .ok()?
        .try_into()
        .ok()
}

/// Seal `{title, body, pane}` for `host`'s key: base64 of nonce ‖ ciphertext ‖ tag (CryptoKit's
/// `AES.GCM.SealedBox.combined`), bound to the host id. Title and body are cut to fit; the body is
/// shortened, then dropped, when the envelope would outgrow the relay's limit.
pub fn seal(key_b64: &str, host: &str, title: &str, body: &str, pane: &str) -> Option<String> {
    use ring::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
    use ring::rand::SecureRandom;
    let key = parse_key(key_b64)?;
    let plain = |body: &str| {
        serde_json::to_vec(&serde_json::json!({
            "title": clip(title, TITLE_MAX),
            "body": body,
            "pane": pane,
        }))
        .unwrap_or_default()
    };
    let mut msg = plain(&clip(body, BODY_MAX));
    if msg.len() > SEALED_PLAIN_MAX {
        msg = plain(&clip(body, 256));
    }
    if msg.len() > SEALED_PLAIN_MAX {
        msg = plain("");
    }
    let mut nonce = [0u8; NONCE_LEN];
    ring::rand::SystemRandom::new().fill(&mut nonce).ok()?;
    let sealer = LessSafeKey::new(UnboundKey::new(&AES_256_GCM, &key).ok()?);
    let mut aad = AAD_PREFIX.to_vec();
    aad.extend_from_slice(host.as_bytes());
    sealer
        .seal_in_place_append_tag(
            Nonce::assume_unique_for_key(nonce),
            Aad::from(aad),
            &mut msg,
        )
        .ok()?;
    let mut out = nonce.to_vec();
    out.extend_from_slice(&msg);
    Some(base64::engine::general_purpose::STANDARD.encode(out))
}

/// Same notification, same id: a newer one for the pane replaces one not yet delivered.
pub fn collapse_id(host: &str, pane: &str) -> String {
    let d = ring::digest::digest(&ring::digest::SHA256, format!("{host}\0{pane}").as_bytes());
    d.as_ref()[..16]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The APNs payload for direct sends: the placeholder alert, `mutable-content` for the app's
/// extension, and the sealed envelope with the key id (the host entry). The relay builds the
/// same from a relay request.
pub fn payload(host: &str, sealed: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "aps": {
            "alert": { "title": PLACEHOLDER_TITLE, "body": PLACEHOLDER_BODY },
            "sound": "default",
            "thread-id": host,
            "mutable-content": 1,
        },
        "sealed": sealed,
        "kid": host,
    }))
    .unwrap_or_default()
}

/// The push relay's request (`POST /v1/notify`).
fn relay_request(reg: &Reg, sealed: &str, collapse: &str) -> serde_json::Value {
    serde_json::json!({
        "token": reg.token,
        "env": reg.env,
        "collapseId": collapse,
        "title": PLACEHOLDER_TITLE,
        "body": PLACEHOLDER_BODY,
        "threadId": reg.host,
        "bundle": reg.topic,
        "sealed": sealed,
        "kid": reg.host,
    })
}

/// One push for the worker.
pub struct Job {
    pub mode: PushMode,
    pub reg: Reg,
    pub sealed: String,
    pub collapse: String,
}

/// APNs (or the relay) said a token is gone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dead {
    pub token: String,
    pub generation: u64,
}

struct Shared {
    queue: Mutex<VecDeque<Job>>,
    ready: Condvar,
    dead: Mutex<Vec<Dead>>,
    dropped: AtomicU64,
    sent: AtomicU64,
    failed: AtomicU64,
}

/// The sending side the App holds. The worker thread starts on the first push.
#[derive(Clone)]
pub struct Pusher {
    shared: Arc<Shared>,
    started: Arc<std::sync::Once>,
    /// APNs base URL override, for tests against a local server.
    base: Option<String>,
}

impl Default for Pusher {
    fn default() -> Self {
        Self::new(std::env::var("COPAD_MUX_APNS_URL").ok())
    }
}

impl Pusher {
    pub fn new(base: Option<String>) -> Self {
        Self {
            shared: Arc::new(Shared {
                queue: Mutex::new(VecDeque::new()),
                ready: Condvar::new(),
                dead: Mutex::new(Vec::new()),
                dropped: AtomicU64::new(0),
                sent: AtomicU64::new(0),
                failed: AtomicU64::new(0),
            }),
            started: Arc::new(std::sync::Once::new()),
            base,
        }
    }

    /// Queue a push (never blocks; a full queue drops its oldest).
    pub fn push(&self, job: Job) {
        let shared = self.shared.clone();
        let base = self.base.clone();
        self.started.call_once(move || {
            let _ = std::thread::Builder::new()
                .name("push".into())
                .spawn(move || worker(shared, base));
        });
        if let Ok(mut q) = self.shared.queue.lock() {
            if q.len() >= QUEUE_CAP {
                q.pop_front();
                self.shared.dropped.fetch_add(1, Ordering::Relaxed);
            }
            q.push_back(job);
        }
        self.shared.ready.notify_one();
    }

    /// Dead tokens reported since the last call.
    pub fn take_dead(&self) -> Vec<Dead> {
        self.shared
            .dead
            .lock()
            .map(|mut d| std::mem::take(&mut *d))
            .unwrap_or_default()
    }

    /// (sent, failed, dropped) since start.
    pub fn counts(&self) -> (u64, u64, u64) {
        (
            self.shared.sent.load(Ordering::Relaxed),
            self.shared.failed.load(Ordering::Relaxed),
            self.shared.dropped.load(Ordering::Relaxed),
        )
    }
}

/// What one send came to.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Sent,
    /// The token will never work again.
    Dead,
    /// Hold off: this device only, or everything, until the instant.
    Wait {
        device: bool,
        secs: u64,
    },
    Failed,
}

/// The relay's answer, by status and error code.
fn relay_outcome(status: u16, code: &str, retry_after: Option<u64>) -> Outcome {
    let secs = retry_after.unwrap_or(60).clamp(1, 3600);
    match status {
        204 => Outcome::Sent,
        410 => Outcome::Dead,
        429 => Outcome::Wait {
            device: code == "device_rate_limited",
            secs,
        },
        503 => Outcome::Wait {
            device: false,
            secs,
        },
        _ => Outcome::Failed,
    }
}

/// The last six characters of a token: enough to tell devices apart in a log, not to send.
fn tail(token: &str) -> &str {
    &token[token.len().saturating_sub(6)..]
}

/// The worker: no retries (a notification is best-effort and the next one for the pane supersedes
/// it through its collapse id), but it honours the relay's Retry-After — for one device, or for
/// everything — by dropping what arrives meanwhile.
fn worker(shared: Arc<Shared>, base: Option<String>) {
    let apns = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(10))
        .http2_prior_knowledge()
        .build();
    let relay = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(10))
        // A redirect would carry the notification somewhere nobody configured.
        .redirect(reqwest::redirect::Policy::none())
        .build();
    let (Ok(apns), Ok(relay)) = (apns, relay) else {
        return;
    };
    let mut jwt: Option<(PushConfig, String, Instant)> = None;
    let mut hold_all: Option<Instant> = None;
    let mut hold: HashMap<String, Instant> = HashMap::new();
    loop {
        let job = {
            let Ok(mut q) = shared.queue.lock() else {
                return;
            };
            loop {
                if let Some(j) = q.pop_front() {
                    break j;
                }
                q = match shared.ready.wait(q) {
                    Ok(q) => q,
                    Err(_) => return,
                };
            }
        };
        let now = Instant::now();
        hold.retain(|_, until| *until > now);
        if hold_all.is_some_and(|t| t > now) || hold.contains_key(&job.reg.token) {
            shared.dropped.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        let outcome = match &job.mode {
            PushMode::Off => continue,
            PushMode::Relay(url) => send_relay(&relay, url, &job),
            PushMode::Direct(cfg) => send_direct(&apns, base.as_deref(), cfg, &mut jwt, &job),
        };
        match outcome {
            Outcome::Sent => {
                shared.sent.fetch_add(1, Ordering::Relaxed);
            }
            Outcome::Dead => {
                shared.failed.fetch_add(1, Ordering::Relaxed);
                if let Ok(mut d) = shared.dead.lock() {
                    d.push(Dead {
                        token: job.reg.token.clone(),
                        generation: job.reg.generation,
                    });
                }
            }
            Outcome::Wait { device, secs } => {
                shared.failed.fetch_add(1, Ordering::Relaxed);
                let until = Instant::now() + Duration::from_secs(secs);
                if device {
                    hold.insert(job.reg.token.clone(), until);
                } else {
                    hold_all = Some(until);
                }
            }
            Outcome::Failed => {
                shared.failed.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

fn send_relay(client: &reqwest::blocking::Client, url: &str, job: &Job) -> Outcome {
    let res = client
        .post(format!("{}/v1/notify", url.trim_end_matches('/')))
        .json(&relay_request(&job.reg, &job.sealed, &job.collapse))
        .send();
    match res {
        Ok(r) => {
            let status = r.status().as_u16();
            let retry = r
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse().ok());
            let code = r
                .json::<serde_json::Value>()
                .ok()
                .and_then(|v| v["error"]["code"].as_str().map(str::to_string))
                .unwrap_or_default();
            let out = relay_outcome(status, &code, retry);
            if matches!(out, Outcome::Failed | Outcome::Wait { .. }) {
                eprintln!(
                    "comux push: relay {status} {code} for …{}",
                    tail(&job.reg.token)
                );
            }
            out
        }
        Err(e) => {
            eprintln!("comux push: relay: {}", e.without_url());
            Outcome::Failed
        }
    }
}

fn send_direct(
    client: &reqwest::blocking::Client,
    base: Option<&str>,
    cfg: &PushConfig,
    jwt: &mut Option<(PushConfig, String, Instant)>,
    job: &Job,
) -> Outcome {
    let fresh = jwt
        .as_ref()
        .is_some_and(|(c, _, at)| c == cfg && at.elapsed() < JWT_TTL);
    if !fresh {
        match sign_jwt(cfg) {
            Ok(t) => *jwt = Some((cfg.clone(), t, Instant::now())),
            Err(e) => {
                eprintln!("comux push: {e}");
                return Outcome::Failed;
            }
        }
    }
    let Some((_, token, _)) = jwt.as_ref() else {
        return Outcome::Failed;
    };
    let host = base.map(str::to_string).unwrap_or_else(|| {
        match job.reg.env {
            ApnsEnv::Sandbox => "https://api.sandbox.push.apple.com",
            ApnsEnv::Production => "https://api.push.apple.com",
        }
        .to_string()
    });
    let res = client
        .post(format!("{host}/3/device/{}", job.reg.token))
        .header("authorization", format!("bearer {token}"))
        .header("apns-topic", &job.reg.topic)
        .header("apns-push-type", "alert")
        .header("apns-priority", "10")
        .header("apns-collapse-id", &job.collapse)
        .body(payload(&job.reg.host, &job.sealed))
        .send();
    match res {
        Ok(r) if r.status().is_success() => Outcome::Sent,
        Ok(r) => {
            let status = r.status().as_u16();
            let reason = r
                .json::<serde_json::Value>()
                .ok()
                .and_then(|v| v["reason"].as_str().map(str::to_string))
                .unwrap_or_default();
            if reason == "ExpiredProviderToken" || reason == "InvalidProviderToken" {
                *jwt = None;
            }
            if is_dead(status, &reason) {
                return Outcome::Dead;
            }
            // The device token is the owner's: never logged whole.
            eprintln!(
                "comux push: APNs {status} {reason} for …{}",
                tail(&job.reg.token)
            );
            Outcome::Failed
        }
        Err(e) => {
            // The URL carries the device token.
            eprintln!("comux push: {}", e.without_url());
            Outcome::Failed
        }
    }
}

/// A response that means the token will never work again.
fn is_dead(status: u16, reason: &str) -> bool {
    status == 410
        || (status == 400 && (reason == "BadDeviceToken" || reason == "DeviceTokenNotForTopic"))
}

fn b64url(b: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

/// The provider token: ES256 over `{alg,kid}.{iss,iat}` with the team's `.p8` key.
pub fn sign_jwt(cfg: &PushConfig) -> Result<String, String> {
    let pem = std::fs::read_to_string(&cfg.key_file)
        .map_err(|e| format!("APNs key {}: {e}", cfg.key_file.display()))?;
    let der: String = pem
        .lines()
        .filter(|l| !l.starts_with("-----"))
        .collect::<String>()
        .split_whitespace()
        .collect();
    let der = base64::engine::general_purpose::STANDARD
        .decode(der)
        .map_err(|_| "the APNs key is not a PEM .p8 file".to_string())?;
    let rng = ring::rand::SystemRandom::new();
    let key = ring::signature::EcdsaKeyPair::from_pkcs8(
        &ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING,
        &der,
        &rng,
    )
    .map_err(|_| "the APNs key is not a P-256 PKCS#8 key".to_string())?;
    let iat = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let head = b64url(
        serde_json::json!({ "alg": "ES256", "kid": cfg.key_id })
            .to_string()
            .as_bytes(),
    );
    let claims = b64url(
        serde_json::json!({ "iss": cfg.team_id, "iat": iat })
            .to_string()
            .as_bytes(),
    );
    let input = format!("{head}.{claims}");
    let sig = key
        .sign(&rng, input.as_bytes())
        .map_err(|_| "signing the APNs token failed".to_string())?;
    Ok(format!("{input}.{}", b64url(sig.as_ref())))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOK_A: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90";
    const TOK_B: &str = "ffeeddccbbaa99887766554433221100ffeeddccbbaa99887766554433221100";

    const KEY: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";

    fn new<'a>(
        token: &'a str,
        env: ApnsEnv,
        topic: &'a str,
        host: &'a str,
        device: &'a str,
    ) -> NewReg<'a> {
        NewReg {
            token,
            env,
            topic,
            host,
            device,
            key: KEY,
        }
    }

    fn reg() -> (Registry, PathBuf) {
        // macOS clocks tick in microseconds: parallel tests need more than the time to differ.
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "comux-push-{}-{}-{}.json",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        (Registry::load(p.clone()), p)
    }

    #[test]
    fn one_push_per_device_routed_to_the_latest_host_entry() {
        let (mut r, p) = reg();
        r.register(new(TOK_A, ApnsEnv::Sandbox, "com.x.app", "host-1", "phone"))
            .unwrap();
        r.register(new(
            TOK_B,
            ApnsEnv::Production,
            "com.x.app",
            "host-1",
            "pad",
        ))
        .unwrap();
        r.register(new(TOK_A, ApnsEnv::Sandbox, "com.x.app", "host-2", "phone"))
            .unwrap();
        let t = r.targets();
        assert_eq!(t.len(), 2, "one per token");
        let a = t.iter().find(|x| x.token == TOK_A).unwrap();
        assert_eq!(a.host, "host-2", "the entry registered last wins");
        // Re-registering host-1 makes it the destination again.
        r.register(new(TOK_A, ApnsEnv::Sandbox, "com.x.app", "host-1", "phone"))
            .unwrap();
        assert_eq!(
            r.targets().iter().find(|x| x.token == TOK_A).unwrap().host,
            "host-1"
        );
        // Persisted.
        assert_eq!(Registry::load(p.clone()).len(), 3);
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn a_dead_report_spares_a_registration_refreshed_since() {
        let (mut r, p) = reg();
        r.register(new(TOK_A, ApnsEnv::Sandbox, "com.x.app", "h", ""))
            .unwrap();
        let old = r.targets()[0].generation;
        r.register(new(TOK_A, ApnsEnv::Sandbox, "com.x.app", "h", ""))
            .unwrap();
        r.drop_dead(TOK_A, old);
        assert_eq!(r.len(), 1, "refreshed after the failing push: kept");
        let now = r.targets()[0].generation;
        r.drop_dead(TOK_A, now);
        assert!(r.is_empty());
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn bad_input_is_refused_not_stored() {
        let (mut r, _) = reg();
        assert!(
            r.register(new("zz", ApnsEnv::Sandbox, "com.x", "h", ""))
                .is_err()
        );
        assert!(
            r.register(new(TOK_A, ApnsEnv::Sandbox, "com.x/../y", "h", ""))
                .is_err()
        );
        assert!(
            r.register(new(TOK_A, ApnsEnv::Sandbox, "com.x", "", ""))
                .is_err()
        );
        assert!(r.is_empty());
    }

    /// What the app's extension does (CryptoKit `AES.GCM.open` on the combined box).
    fn open(key_b64: &str, host: &str, sealed: &str) -> Option<serde_json::Value> {
        use ring::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
        let key = parse_key(key_b64)?;
        let raw = base64::engine::general_purpose::STANDARD
            .decode(sealed)
            .ok()?;
        let (nonce, rest) = raw.split_at(12);
        let mut buf = rest.to_vec();
        let opener = LessSafeKey::new(UnboundKey::new(&AES_256_GCM, &key).ok()?);
        let mut aad = AAD_PREFIX.to_vec();
        aad.extend_from_slice(host.as_bytes());
        let plain = opener
            .open_in_place(
                Nonce::try_assume_unique_for_key(nonce).ok()?,
                Aad::from(aad),
                &mut buf,
            )
            .ok()?;
        serde_json::from_slice(plain).ok()
    }

    #[test]
    fn a_sealed_push_opens_only_with_its_key_and_host() {
        let sealed = seal(KEY, "host-1", "Claude finished", "본문", "nonce-7").unwrap();
        let v = open(KEY, "host-1", &sealed).expect("opens");
        assert_eq!(v["title"], "Claude finished");
        assert_eq!(v["body"], "본문");
        assert_eq!(v["pane"], "nonce-7");
        assert!(
            open(KEY, "host-2", &sealed).is_none(),
            "bound to the host id"
        );
        let other = "HwECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";
        assert!(open(other, "host-1", &sealed).is_none());
        assert_ne!(
            sealed,
            seal(KEY, "host-1", "Claude finished", "본문", "nonce-7").unwrap(),
            "a fresh nonce every time"
        );
        assert!(seal("short", "h", "t", "b", "p").is_none());
    }

    /// A fixed envelope (key 0..31, made with Python's `cryptography`); copad-ios'
    /// `PushEnvelopeTests` opens the same bytes, so both sides agree on the format.
    const VECTOR: &str = "ZGVmZ2hpamtsbW5vMzm8CR2QdKQciexQMcnS327gdmvlCdFIhb/DJpjGiH+2xWK0JWb3Yd/JWnyRI7+rfnv+jk4jMkCflY6rYoNG4P6NsvSrWFyi8wUC4g==";

    #[test]
    fn the_shared_vector_opens() {
        let v = open(KEY, "host-1", VECTOR).expect("opens");
        assert_eq!(v["title"], "Claude finished");
        assert_eq!(v["body"], "본문");
        assert_eq!(v["pane"], "nonce-7");
    }

    #[test]
    fn the_envelope_fits_the_relay_and_the_payload_apns() {
        for body in ["한글 본문 ".repeat(2000), "\u{1}".repeat(1024)] {
            let sealed = seal(KEY, "host-1", &"t".repeat(500), &body, "nonce-7").unwrap();
            assert!(sealed.len() <= 3000, "relay limit: {}", sealed.len());
            let v = open(KEY, "host-1", &sealed).unwrap();
            assert_eq!(v["pane"], "nonce-7", "the routing always survives");
            assert!(v["title"].as_str().unwrap().len() <= TITLE_MAX);
            let p = payload("host-1", &sealed);
            assert!(p.len() <= 4096);
            let p: serde_json::Value = serde_json::from_slice(&p).unwrap();
            assert_eq!(p["aps"]["mutable-content"], 1);
            assert_eq!(p["aps"]["alert"]["title"], PLACEHOLDER_TITLE);
            assert_eq!(p["kid"], "host-1");
            assert!(p.get("comux").is_none(), "no routing in the clear");
        }
    }

    #[test]
    fn keyless_registrations_load_but_are_never_sent_to() {
        let (_, p) = reg();
        std::fs::write(
            &p,
            format!(
                r#"{{"regs":[{{"token":"{TOK_A}","env":"sandbox","topic":"com.x.app","host":"h","generation":1}}],"next":1}}"#
            ),
        )
        .unwrap();
        let mut r = Registry::load(p.clone());
        assert_eq!(r.len(), 1, "an old file still loads");
        assert!(r.targets().is_empty(), "but nothing goes out in the clear");
        r.register(new(TOK_A, ApnsEnv::Sandbox, "com.x.app", "h", ""))
            .unwrap();
        assert_eq!(r.targets().len(), 1, "re-registering replaces it");
        let mut keyless = new(TOK_B, ApnsEnv::Sandbox, "com.x.app", "h", "");
        keyless.key = "";
        assert!(
            r.register(keyless).is_err(),
            "an app without a key is refused"
        );
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn the_relay_request_and_its_answers() {
        let (mut r, p) = reg();
        r.register(new(TOK_A, ApnsEnv::Sandbox, "com.x.app", "host-1", ""))
            .unwrap();
        let reg = &r.targets()[0];
        let v = relay_request(reg, "c2VhbGVk", &collapse_id("host-1", "pane"));
        assert_eq!(v["env"], "sandbox");
        assert_eq!(v["bundle"], "com.x.app");
        assert_eq!(v["kid"], "host-1");
        assert_eq!(v["title"], PLACEHOLDER_TITLE);
        let c = v["collapseId"].as_str().unwrap();
        assert!(
            c.len() == 32
                && c.bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        );
        assert_eq!(relay_outcome(204, "", None), Outcome::Sent);
        assert_eq!(relay_outcome(410, "unregistered", None), Outcome::Dead);
        assert_eq!(
            relay_outcome(429, "device_rate_limited", Some(30)),
            Outcome::Wait {
                device: true,
                secs: 30
            }
        );
        assert_eq!(
            relay_outcome(429, "rate_limited", None),
            Outcome::Wait {
                device: false,
                secs: 60
            }
        );
        assert_eq!(
            relay_outcome(503, "unavailable", Some(99999)),
            Outcome::Wait {
                device: false,
                secs: 3600
            }
        );
        // A 200 is some other page at a wrong URL, not a delivery.
        assert_eq!(relay_outcome(200, "", None), Outcome::Failed);
        assert_eq!(relay_outcome(400, "bad_request", None), Outcome::Failed);
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn relay_urls_are_https_or_local() {
        assert!(check_relay_url("https://aide-push.marshallku.dev").is_ok());
        assert!(check_relay_url("http://127.0.0.1:18090").is_ok());
        assert!(check_relay_url("http://example.com").is_err());
        assert!(check_relay_url("https://x.dev/?a=1").is_err());
        assert!(check_relay_url("nope").is_err());
    }

    #[test]
    fn dead_tokens_are_recognized() {
        assert!(is_dead(410, "Unregistered"));
        assert!(is_dead(400, "BadDeviceToken"));
        assert!(!is_dead(400, "PayloadTooLarge"));
        assert!(!is_dead(429, "TooManyRequests"));
    }

    #[test]
    fn the_provider_token_is_es256_and_verifies() {
        use ring::signature::KeyPair;
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = ring::signature::EcdsaKeyPair::generate_pkcs8(
            &ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING,
            &rng,
        )
        .unwrap();
        let pem = format!(
            "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
            base64::engine::general_purpose::STANDARD.encode(pkcs8.as_ref())
        );
        let (_, path) = reg();
        std::fs::write(&path, pem).unwrap();
        let cfg = PushConfig {
            key_file: path.clone(),
            key_id: "KEY123".into(),
            team_id: "TEAM456".into(),
        };
        let jwt = sign_jwt(&cfg).unwrap();
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);
        let dec = |s: &str| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(s)
                .unwrap()
        };
        let head: serde_json::Value = serde_json::from_slice(&dec(parts[0])).unwrap();
        assert_eq!(head["alg"], "ES256");
        assert_eq!(head["kid"], "KEY123");
        let claims: serde_json::Value = serde_json::from_slice(&dec(parts[1])).unwrap();
        assert_eq!(claims["iss"], "TEAM456");
        let pair = ring::signature::EcdsaKeyPair::from_pkcs8(
            &ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING,
            pkcs8.as_ref(),
            &rng,
        )
        .unwrap();
        let public = ring::signature::UnparsedPublicKey::new(
            &ring::signature::ECDSA_P256_SHA256_FIXED,
            pair.public_key().as_ref().to_vec(),
        );
        public
            .verify(
                format!("{}.{}", parts[0], parts[1]).as_bytes(),
                &dec(parts[2]),
            )
            .expect("signature verifies");
        let _ = std::fs::remove_file(path);
    }
}
