//! Agent notifications pushed to phones over APNs (decisions #175).
//!
//! The copad iOS app registers its device token through the L0 command channel
//! (`push-register`); this server keeps the registrations on disk and, for every notification it
//! raises, hands one push per device to a worker thread that talks HTTP/2 to Apple. Nothing here
//! runs on the server loop: the loop only enqueues, and learns about dead tokens by polling.
//!
//! Credentials are the team's APNs auth key (`[push]` in mux.toml): a `.p8` PKCS#8 P-256 key,
//! its key id and the team id. The JWT is signed with `ring` and reused for 50 minutes (Apple
//! refuses one older than an hour and throttles ones refreshed more often than every 20).

use std::collections::VecDeque;
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
/// Apple's limit for an alert payload.
const PAYLOAD_MAX: usize = 4096;
const JWT_TTL: Duration = Duration::from_secs(50 * 60);

/// `[push]` in mux.toml.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushConfig {
    pub key_file: PathBuf,
    pub key_id: String,
    pub team_id: String,
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
    pub fn register(
        &mut self,
        token: &str,
        env: ApnsEnv,
        topic: &str,
        host: &str,
        device: &str,
    ) -> Result<(), String> {
        let token = token.to_ascii_lowercase();
        if !valid_token(&token) {
            return Err("not an APNs device token".into());
        }
        if !valid_word(topic, 200) || !valid_word(host, 80) {
            return Err("bad topic or host id".into());
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
    /// connected through), so a device never gets the same notification twice.
    pub fn targets(&self) -> Vec<Reg> {
        let mut out: Vec<Reg> = Vec::new();
        for r in &self.st.regs {
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

/// The APNs payload: alert + routing (`comux.host`, `comux.pane`). Bounded to Apple's 4 KB —
/// title and body are cut first, and the body is dropped if that is still not enough; the
/// routing fields always survive.
pub fn payload(title: &str, body: &str, host: &str, pane: &str) -> Vec<u8> {
    let build = |body: Option<&str>| {
        let mut alert = serde_json::json!({ "title": clip(title, TITLE_MAX) });
        if let Some(b) = body {
            alert["body"] = serde_json::Value::String(clip(b, BODY_MAX));
        }
        serde_json::to_vec(&serde_json::json!({
            "aps": { "alert": alert, "sound": "default", "thread-id": host },
            "comux": { "host": host, "pane": pane },
        }))
        .unwrap_or_default()
    };
    let full = build(Some(body));
    if full.len() <= PAYLOAD_MAX {
        full
    } else {
        build(None)
    }
}

/// One push for the worker.
pub struct Job {
    pub cfg: PushConfig,
    pub reg: Reg,
    pub payload: Vec<u8>,
    /// `apns-collapse-id`: a newer notification for the same pane replaces an undelivered one.
    pub collapse: String,
}

/// APNs said a token is gone (410, or 400 BadDeviceToken).
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
    /// Base URL override, for tests against a local server.
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
                .name("apns".into())
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

/// The worker: one HTTP/2 client, a cached JWT, no retries (a notification is best-effort and
/// the next one for the pane supersedes it through its collapse id).
fn worker(shared: Arc<Shared>, base: Option<String>) {
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(10))
        .http2_prior_knowledge()
        .build();
    let Ok(client) = client else {
        return;
    };
    let mut jwt: Option<(PushConfig, String, Instant)> = None;
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
        let fresh = jwt
            .as_ref()
            .is_some_and(|(c, _, at)| *c == job.cfg && at.elapsed() < JWT_TTL);
        if !fresh {
            match sign_jwt(&job.cfg) {
                Ok(t) => jwt = Some((job.cfg.clone(), t, Instant::now())),
                Err(e) => {
                    eprintln!("comux push: {e}");
                    shared.failed.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
            }
        }
        let Some((_, token, _)) = jwt.as_ref() else {
            continue;
        };
        let host = base.clone().unwrap_or_else(|| {
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
            .header("apns-collapse-id", clip(&job.collapse, 64))
            .body(job.payload)
            .send();
        match res {
            Ok(r) if r.status().is_success() => {
                shared.sent.fetch_add(1, Ordering::Relaxed);
            }
            Ok(r) => {
                shared.failed.fetch_add(1, Ordering::Relaxed);
                let status = r.status().as_u16();
                let reason = r
                    .json::<serde_json::Value>()
                    .ok()
                    .and_then(|v| v["reason"].as_str().map(str::to_string))
                    .unwrap_or_default();
                if is_dead(status, &reason) {
                    if let Ok(mut d) = shared.dead.lock() {
                        d.push(Dead {
                            token: job.reg.token.clone(),
                            generation: job.reg.generation,
                        });
                    }
                } else {
                    // The device token is the owner's: never logged whole.
                    eprintln!(
                        "comux push: APNs {status} {reason} for …{}",
                        &job.reg.token[job.reg.token.len().saturating_sub(6)..]
                    );
                }
                if reason == "ExpiredProviderToken" || reason == "InvalidProviderToken" {
                    jwt = None;
                }
            }
            Err(e) => {
                shared.failed.fetch_add(1, Ordering::Relaxed);
                // The URL carries the device token.
                eprintln!("comux push: {}", e.without_url());
            }
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

    fn reg() -> (Registry, PathBuf) {
        let p = std::env::temp_dir().join(format!(
            "comux-push-{}-{}.json",
            std::process::id(),
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
        r.register(TOK_A, ApnsEnv::Sandbox, "com.x.app", "host-1", "phone")
            .unwrap();
        r.register(TOK_B, ApnsEnv::Production, "com.x.app", "host-1", "pad")
            .unwrap();
        r.register(TOK_A, ApnsEnv::Sandbox, "com.x.app", "host-2", "phone")
            .unwrap();
        let t = r.targets();
        assert_eq!(t.len(), 2, "one per token");
        let a = t.iter().find(|x| x.token == TOK_A).unwrap();
        assert_eq!(a.host, "host-2", "the entry registered last wins");
        // Re-registering host-1 makes it the destination again.
        r.register(TOK_A, ApnsEnv::Sandbox, "com.x.app", "host-1", "phone")
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
        r.register(TOK_A, ApnsEnv::Sandbox, "com.x.app", "h", "")
            .unwrap();
        let old = r.targets()[0].generation;
        r.register(TOK_A, ApnsEnv::Sandbox, "com.x.app", "h", "")
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
            r.register("zz", ApnsEnv::Sandbox, "com.x", "h", "")
                .is_err()
        );
        assert!(
            r.register(TOK_A, ApnsEnv::Sandbox, "com.x/../y", "h", "")
                .is_err()
        );
        assert!(
            r.register(TOK_A, ApnsEnv::Sandbox, "com.x", "", "")
                .is_err()
        );
        assert!(r.is_empty());
    }

    #[test]
    fn the_payload_stays_under_four_kilobytes_and_keeps_its_routing() {
        let body = "한글 본문 ".repeat(2000);
        let p = payload(&"t".repeat(500), &body, "host-1", "nonce-7");
        assert!(p.len() <= PAYLOAD_MAX);
        let v: serde_json::Value = serde_json::from_slice(&p).unwrap();
        assert_eq!(v["comux"]["pane"], "nonce-7");
        assert_eq!(v["comux"]["host"], "host-1");
        let b = v["aps"]["alert"]["body"].as_str().unwrap();
        assert!(b.len() <= BODY_MAX && b.ends_with('…'));
        assert!(v["aps"]["alert"]["title"].as_str().unwrap().len() <= TITLE_MAX);
        // A body that is all escapes still fits: the body goes, the routing stays.
        let nasty = "\u{1}".repeat(1024);
        let p = payload("t", &nasty, "h", "p");
        assert!(p.len() <= PAYLOAD_MAX);
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
