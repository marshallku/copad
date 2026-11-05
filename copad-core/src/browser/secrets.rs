//! The credential boundary: metadata crosses the agent wire, secrets never do.
//!
//! The agent-visible type is [`CredentialRef`], which has no field that can hold secret material.
//! The secret lives only in the platform keychain, keyed by [`CredentialRef::id`], and travels
//! into the page inside the GUI process; it appears in no request, response, event or log.
//!
//! A "seal" (fill, then refuse read-back RPCs while the secret sits in the DOM) was rejected: the
//! agent can install a listener before the fill that copies the value to `localStorage` and read
//! the copy once the lock lifts, and no transition proves the secret is gone (cancelled submit,
//! SPA swallowing the event, back-navigation). So a credential only enters a document automation
//! was never in, [`TabMode::Protected`], which the GUI enters by destroying and rebuilding the
//! web view.
//!
//! Promised: the secret never crosses the agent boundary, and no agent-installed script is in
//! the document holding it. Not promised: safety from a page that is itself hostile.

use serde::{Deserialize, Serialize};

use crate::protocol::ResponseError;

/// Which field of a login form a credential fills. A password credential may only be written
/// into an `<input type="password">`; the check lives in [`validate_fill`].
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CredentialSlot {
    Username,
    Password,
}

/// Metadata only: this crosses the RPC boundary and is what an agent can see. A secret-bearing
/// field here would undo the whole design.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct CredentialRef {
    pub id: String,
    pub origin: String,
    pub username: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub slot: CredentialSlot,
    #[serde(default)]
    pub created_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used: Option<u64>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum TabMode {
    #[default]
    Automation,
    /// Freshly rebuilt web view, no agent script present, no capture installed. Agent reads are
    /// refused; credential fills are allowed.
    Protected,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabState {
    pub tab_id: String,
    pub profile: String,
    pub mode: TabMode,
    pub origin: String,
    /// Bumped on every main-frame commit and mode change; binds an authorization to a document
    /// rather than an origin.
    pub document_generation: u64,
}

/// An in-flight `browser.secret.fill`, captured at approval time and re-validated at injection
/// time. The tab can navigate during the async keychain read, so every field is compared again
/// against the live [`TabState`] right before the value touches the page; any mismatch aborts and
/// the secret is dropped unwritten.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FillRequest {
    pub credential_id: String,
    pub profile: String,
    pub tab_id: String,
    pub origin: String,
    pub document_generation: u64,
    pub slot: CredentialSlot,
}

/// The target element as observed right now, in the same synchronous step that performs the
/// injection. Deliberately not a field of [`FillRequest`]: a value captured at approval time is
/// stale after the keychain read, and a page can swap the element or flip its `type` without a
/// navigation, leaving `document_generation` unchanged.
///
/// Platform contract: observe the element, call [`validate_fill`], and write into that same
/// element with no `await` between the three.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveTarget {
    pub selector: String,
    pub is_password_input: bool,
}

fn err(code: &str, message: impl Into<String>) -> ResponseError {
    ResponseError {
        code: code.to_string(),
        message: message.into(),
    }
}

/// Re-validate a fill immediately before injection. Enforces, in order: the tab still exists in
/// the same profile, it is still `Protected`, the document has not changed, the origin matches
/// the credential exactly (no subdomain or scheme widening), and a password credential goes into
/// a password input.
pub fn validate_fill(
    req: &FillRequest,
    cred: &CredentialRef,
    live: &TabState,
    target: &LiveTarget,
) -> Result<(), ResponseError> {
    if req.credential_id != cred.id {
        return Err(err(
            "credential_mismatch",
            "request does not name this credential",
        ));
    }
    // Re-check the approval's origin and slot against the credential as it stands NOW: the index
    // is a file, and a rewrite between approval and injection could turn an approved `Username`
    // into a `Password` injected on the strength of an approval for something else.
    if !origin_matches(&req.origin, &cred.origin) {
        return Err(err(
            "approval_mismatch",
            "the credential's origin changed after approval",
        ));
    }
    if req.slot != cred.slot {
        return Err(err(
            "approval_mismatch",
            "the credential's slot changed after approval",
        ));
    }
    if req.tab_id != live.tab_id || req.profile != live.profile {
        return Err(err(
            "tab_changed",
            "target tab is no longer the approved one",
        ));
    }
    if live.mode != TabMode::Protected {
        return Err(err(
            "requires_protected",
            "a credential may only be filled into a protected tab",
        ));
    }
    if req.document_generation != live.document_generation {
        return Err(err(
            "document_changed",
            "the tab navigated after approval — fill aborted",
        ));
    }
    if !origin_matches(&cred.origin, &live.origin) {
        return Err(err(
            "origin_mismatch",
            format!(
                "credential is bound to {} but the tab is on {}",
                cred.origin, live.origin
            ),
        ));
    }
    if cred.slot == CredentialSlot::Password && !target.is_password_input {
        return Err(err(
            "not_a_password_input",
            "a password credential may only be filled into <input type=\"password\">",
        ));
    }
    Ok(())
}

/// Exact, case-insensitive origin equality, deliberately not a suffix or subdomain match:
/// `https://evil.github.com` must not get a credential bound to `https://github.com`, nor
/// `http://` one bound to `https://`.
pub fn origin_matches(credential_origin: &str, tab_origin: &str) -> bool {
    !credential_origin.is_empty()
        && !tab_origin.is_empty()
        && credential_origin.eq_ignore_ascii_case(tab_origin)
}

pub fn credential_index_path() -> std::path::PathBuf {
    crate::paths::state_dir()
        .join("browser")
        .join("credentials.json")
}

/// Error returned when the platform keychain is unavailable. There is no plaintext fallback:
/// decision #54's plaintext store is for revocable Slack bot tokens, whereas website passwords
/// are not revocable in one click and a 0600 file is readable by the same-user agent this module
/// keeps them from. An unavailable backend disables the feature rather than downgrading it.
pub fn backend_unavailable(detail: &str) -> ResponseError {
    err(
        "secret_backend_unavailable",
        format!("platform keychain unavailable, password features are disabled: {detail}"),
    )
}

/// The credential index on disk: a list of [`CredentialRef`], so no secret material by
/// construction. Still `0600`, because the set of sites you have accounts on is worth protecting.
pub fn load_index() -> Vec<CredentialRef> {
    let path = credential_index_path();
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    serde_json::from_str(&raw).unwrap_or_else(|e| {
        eprintln!("[copad] credential index parse failed: {e}");
        Vec::new()
    })
}

/// Replace the index via temp + rename, so a crash cannot leave a truncated list that silently
/// "forgets" credentials whose secrets are still in the keychain.
pub fn save_index(entries: &[CredentialRef]) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let path = credential_index_path();
    let dir = path
        .parent()
        .ok_or_else(|| "credential index has no parent directory".to_string())?;
    std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));

    let body = serde_json::to_string_pretty(entries).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| format!("open {}: {e}", tmp.display()))?;
        f.write_all(body.as_bytes())
            .map_err(|e| format!("write {}: {e}", tmp.display()))?;
        f.sync_all()
            .map_err(|e| format!("fsync {}: {e}", tmp.display()))?;
    }
    std::fs::rename(&tmp, &path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("rename into {}: {e}", path.display())
    })
}

pub fn upsert(entry: CredentialRef) -> Result<Vec<CredentialRef>, String> {
    let mut all = load_index();
    all.retain(|c| c.id != entry.id);
    all.push(entry);
    all.sort_by(|a, b| a.id.cmp(&b.id));
    save_index(&all)?;
    Ok(all)
}

/// Forget an entry; returns whether one was there. The caller deletes the keychain secret FIRST,
/// since an index entry removed while its secret survives is an orphan nothing can clean up.
pub fn remove(id: &str) -> Result<bool, String> {
    let mut all = load_index();
    let before = all.len();
    all.retain(|c| c.id != id);
    let removed = all.len() != before;
    if removed {
        save_index(&all)?;
    }
    Ok(removed)
}

pub fn list(origin: Option<&str>) -> Vec<CredentialRef> {
    let all = load_index();
    match origin {
        Some(o) => all
            .into_iter()
            .filter(|c| origin_matches(&c.origin, o))
            .collect(),
        None => all,
    }
}

/// A credential id is a keychain account string and must not be mistakable for a path. Permissive
/// about shape (conventionally `host/username`), strict about control characters, whitespace and
/// length.
pub fn is_valid_credential_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 256
        && !id.contains("..")
        && id
            .chars()
            .all(|c| !c.is_control() && c != '\\' && !c.is_whitespace())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cred() -> CredentialRef {
        CredentialRef {
            id: "github.com/marshallku".into(),
            origin: "https://github.com".into(),
            username: "marshallku".into(),
            label: None,
            slot: CredentialSlot::Password,
            created_at: 0,
            last_used: None,
        }
    }

    fn live(mode: TabMode, generation: u64, origin: &str) -> TabState {
        TabState {
            tab_id: "t1".into(),
            profile: "default".into(),
            mode,
            origin: origin.into(),
            document_generation: generation,
        }
    }

    fn req() -> FillRequest {
        FillRequest {
            credential_id: "github.com/marshallku".into(),
            profile: "default".into(),
            tab_id: "t1".into(),
            origin: "https://github.com".into(),
            document_generation: 4,
            slot: CredentialSlot::Password,
        }
    }

    fn target(is_password: bool) -> LiveTarget {
        LiveTarget {
            selector: "#password".into(),
            is_password_input: is_password,
        }
    }

    fn ok_tab() -> TabState {
        live(TabMode::Protected, 4, "https://github.com")
    }

    #[test]
    fn a_matching_protected_document_accepts_the_fill() {
        assert!(validate_fill(&req(), &cred(), &ok_tab(), &target(true)).is_ok());
    }

    #[test]
    fn an_unprotected_tab_is_refused() {
        let tab = live(TabMode::Automation, 4, "https://github.com");
        assert_eq!(
            validate_fill(&req(), &cred(), &tab, &target(true))
                .unwrap_err()
                .code,
            "requires_protected"
        );
    }

    #[test]
    fn a_navigation_between_approval_and_injection_aborts_the_fill() {
        let tab = live(TabMode::Protected, 5, "https://github.com");
        assert_eq!(
            validate_fill(&req(), &cred(), &tab, &target(true))
                .unwrap_err()
                .code,
            "document_changed"
        );
    }

    #[test]
    fn a_lookalike_subdomain_does_not_receive_the_credential() {
        let tab = live(TabMode::Protected, 4, "https://evil.github.com");
        assert_eq!(
            validate_fill(&req(), &cred(), &tab, &target(true))
                .unwrap_err()
                .code,
            "origin_mismatch"
        );
    }

    #[test]
    fn downgrading_the_scheme_does_not_receive_the_credential() {
        let tab = live(TabMode::Protected, 4, "http://github.com");
        assert_eq!(
            validate_fill(&req(), &cred(), &tab, &target(true))
                .unwrap_err()
                .code,
            "origin_mismatch"
        );
    }

    #[test]
    fn a_password_will_not_go_into_a_non_password_input() {
        // The check reads the element as it is NOW, so a page that swapped the input after
        // approval (no navigation, so `document_generation` is unchanged) is still caught.
        assert_eq!(
            validate_fill(&req(), &cred(), &ok_tab(), &target(false))
                .unwrap_err()
                .code,
            "not_a_password_input"
        );
    }

    #[test]
    fn a_username_credential_may_go_into_an_ordinary_input() {
        let mut c = cred();
        c.slot = CredentialSlot::Username;
        let mut r = req();
        r.slot = CredentialSlot::Username;
        assert!(validate_fill(&r, &c, &ok_tab(), &target(false)).is_ok());
    }

    #[test]
    fn a_credential_rewritten_after_approval_is_refused() {
        // The index was rewritten between approval and injection, so the captured approval no
        // longer describes what would be injected.
        let mut c = cred();
        c.slot = CredentialSlot::Username;
        assert_eq!(
            validate_fill(&req(), &c, &ok_tab(), &target(true))
                .unwrap_err()
                .code,
            "approval_mismatch"
        );
    }

    #[test]
    fn a_credential_whose_origin_moved_after_approval_is_refused() {
        let mut c = cred();
        c.origin = "https://gitlab.com".into();
        let tab = live(TabMode::Protected, 4, "https://gitlab.com");
        assert_eq!(
            validate_fill(&req(), &c, &tab, &target(true))
                .unwrap_err()
                .code,
            "approval_mismatch"
        );
    }

    #[test]
    fn a_retargeted_tab_is_refused() {
        let mut tab = ok_tab();
        tab.tab_id = "t2".into();
        assert_eq!(
            validate_fill(&req(), &cred(), &tab, &target(true))
                .unwrap_err()
                .code,
            "tab_changed"
        );
    }

    #[test]
    fn origin_match_is_case_insensitive_but_never_a_suffix_match() {
        assert!(origin_matches("https://GitHub.com", "https://github.com"));
        assert!(!origin_matches(
            "https://github.com",
            "https://github.com.evil.net"
        ));
        assert!(!origin_matches("", ""));
    }

    #[test]
    fn the_agent_facing_type_serializes_without_any_secret_field() {
        // Guards against someone adding a `password` field to the type the agent sees.
        let json = serde_json::to_string(&cred()).unwrap();
        for forbidden in ["password", "secret", "value", "token"] {
            assert!(
                !json.contains(&format!("\"{forbidden}\":")),
                "CredentialRef must not serialize a `{forbidden}` field: {json}"
            );
        }
    }

    #[test]
    fn the_approval_type_cannot_carry_a_stale_target_observation() {
        // `FillRequest` is the approval and must not hold anything about the element, or a GUI
        // could pass a value captured before the keychain await.
        let debug = format!("{:?}", req());
        assert!(
            !debug.contains("password_input"),
            "FillRequest must not carry a target observation: {debug}"
        );
    }

    fn with_temp_state<T>(tag: &str, f: impl FnOnce() -> T) -> T {
        let _guard = super::super::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let root = std::env::temp_dir().join(format!("copad-cred-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let prev_home = std::env::var("HOME").ok();
        let prev_xdg = std::env::var("XDG_STATE_HOME").ok();
        unsafe {
            std::env::set_var("HOME", &root);
            std::env::set_var("XDG_STATE_HOME", root.join("state"));
        }
        let out = f();
        unsafe {
            match prev_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            match prev_xdg {
                Some(v) => std::env::set_var("XDG_STATE_HOME", v),
                None => std::env::remove_var("XDG_STATE_HOME"),
            }
        }
        let _ = std::fs::remove_dir_all(&root);
        out
    }

    #[test]
    fn the_index_round_trips_and_holds_no_secret() {
        with_temp_state("roundtrip", || {
            upsert(cred()).unwrap();
            let all = load_index();
            assert_eq!(all.len(), 1);
            assert_eq!(all[0].id, "github.com/marshallku");
            // Whatever reaches disk cannot carry the password because the TYPE cannot. Checked
            // against the object's keys, not a substring: `slot: "password"` is a field name and
            // is right to persist.
            let raw = std::fs::read_to_string(credential_index_path()).unwrap();
            let parsed: Vec<serde_json::Map<String, serde_json::Value>> =
                serde_json::from_str(&raw).expect("index is a JSON array of objects");
            for entry in &parsed {
                for key in entry.keys() {
                    assert!(
                        !matches!(key.as_str(), "password" | "secret" | "value" | "token"),
                        "the credential index must not persist a `{key}` field: {raw}"
                    );
                }
            }
        });
    }

    #[test]
    fn the_index_file_is_0600() {
        with_temp_state("mode", || {
            use std::os::unix::fs::PermissionsExt;
            upsert(cred()).unwrap();
            let m = std::fs::metadata(credential_index_path()).unwrap();
            assert_eq!(m.permissions().mode() & 0o777, 0o600);
        });
    }

    #[test]
    fn upsert_replaces_rather_than_duplicating() {
        with_temp_state("upsert", || {
            upsert(cred()).unwrap();
            let mut updated = cred();
            updated.username = "renamed".into();
            let all = upsert(updated).unwrap();
            assert_eq!(all.len(), 1);
            assert_eq!(all[0].username, "renamed");
        });
    }

    #[test]
    fn remove_reports_whether_anything_was_there() {
        with_temp_state("remove", || {
            upsert(cred()).unwrap();
            assert!(remove("github.com/marshallku").unwrap());
            assert!(!remove("github.com/marshallku").unwrap());
            assert!(load_index().is_empty());
        });
    }

    #[test]
    fn list_filters_by_exact_origin() {
        with_temp_state("list", || {
            upsert(cred()).unwrap();
            let mut other = cred();
            other.id = "gitlab.com/me".into();
            other.origin = "https://gitlab.com".into();
            upsert(other).unwrap();

            assert_eq!(list(None).len(), 2);
            assert_eq!(list(Some("https://github.com")).len(), 1);
            // Exact, never a suffix match, same as the fill validator, so listing cannot suggest
            // a credential that could not then be filled.
            assert!(list(Some("https://evil.github.com")).is_empty());
        });
    }

    #[test]
    fn a_corrupt_index_reads_as_empty_rather_than_failing_startup() {
        with_temp_state("corrupt", || {
            let path = credential_index_path();
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, b"{not json").unwrap();
            assert!(load_index().is_empty());
        });
    }

    #[test]
    fn credential_ids_reject_control_characters_and_traversal() {
        assert!(is_valid_credential_id("github.com/marshallku"));
        assert!(!is_valid_credential_id(""));
        assert!(!is_valid_credential_id("a/../b"));
        assert!(!is_valid_credential_id("a\nb"));
        assert!(!is_valid_credential_id("a b"));
        assert!(!is_valid_credential_id(&"x".repeat(257)));
    }
}
