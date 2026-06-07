//! Dashboard operator login — first-run admin setup, Argon2id password
//! storage, and HTTP-only session cookies.
//!
//! The historical dashboard authenticated by pasting the bridge bearer
//! token (or the one-time `/v1/auth/token` bootstrap behind a setup
//! token). That is fine for `curl` but a poor operator experience for a
//! real web UI. This module adds a username/password login on top:
//!
//! - First run: `POST /v1/auth/setup` creates the single admin account
//!   (username + Argon2id PHC hash, persisted next to the bridge token).
//! - `POST /v1/auth/login` verifies the password and mints a session.
//! - `POST /v1/auth/logout` drops the session.
//! - `GET  /v1/auth/me` returns the logged-in username.
//! - `GET  /v1/auth/status` reports whether setup is needed / who's in.
//!
//! A successful setup/login sets an **HTTP-only** `relix_session` cookie.
//! The bridge auth middleware (`crate::auth`) admits a request carrying a
//! valid session cookie exactly as if it presented the bridge token, so
//! every dashboard `fetch` authenticates automatically — no token paste.
//!
//! Sessions live in memory (a single-process bridge); they reset on
//! restart, which simply re-prompts the operator to log in. The admin
//! credential is durable on disk.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use argon2::Argon2;
use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use axum::Json;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use rand::RngCore;
use serde::{Deserialize, Serialize};

use crate::config::AppState;

/// Name of the HTTP-only session cookie the dashboard rides on.
pub const SESSION_COOKIE: &str = "relix_session";

/// Session lifetime. A logged-in operator stays authenticated for this
/// long without re-entering their password.
const SESSION_TTL_SECS: i64 = 12 * 60 * 60;

/// Minimum password length accepted at setup. Deliberately modest — this
/// guards a loopback operator console, not an internet service.
const MIN_PASSWORD_LEN: usize = 8;

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ── Admin credential (durable) ──────────────────────────────────

/// The single dashboard admin account, persisted as JSON next to the
/// bridge token. `hash` is an Argon2id PHC string.
#[derive(Clone, Serialize, Deserialize)]
struct AdminRecord {
    username: String,
    hash: String,
    #[serde(default)]
    created_at: i64,
}

/// File-backed admin credential store.
#[derive(Clone)]
struct AdminStore {
    path: Arc<PathBuf>,
    /// Cached record; `None` until first run completes.
    cached: Arc<RwLock<Option<AdminRecord>>>,
}

impl AdminStore {
    fn load(path: &Path) -> Self {
        let cached = std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice::<AdminRecord>(&b).ok());
        Self {
            path: Arc::new(path.to_path_buf()),
            cached: Arc::new(RwLock::new(cached)),
        }
    }

    fn exists(&self) -> bool {
        self.cached.read().map(|c| c.is_some()).unwrap_or(false)
    }

    #[cfg(test)]
    fn username(&self) -> Option<String> {
        self.cached
            .read()
            .ok()
            .and_then(|c| c.as_ref().map(|r| r.username.clone()))
    }

    /// Create the admin account (first run only). Hashes `password` with
    /// Argon2id and persists the record at restrictive permissions.
    fn create(&self, username: &str, password: &str) -> Result<(), String> {
        let rec = write_admin_record(&self.path, username, password)?;
        if let Ok(mut c) = self.cached.write() {
            *c = Some(rec);
        }
        Ok(())
    }

    /// Verify a login. Returns the canonical username on success.
    fn verify(&self, username: &str, password: &str) -> Option<String> {
        let rec = self.cached.read().ok()?.clone()?;
        if rec.username != username {
            return None;
        }
        let parsed = PasswordHash::new(&rec.hash).ok()?;
        Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .ok()
            .map(|_| rec.username)
    }
}

/// Where the dashboard admin credential lives, given the bridge-token path:
/// `dashboard-admin.json` in the SAME directory. Used by both the running
/// bridge ([`DashboardAuth::from_token_path`]) and the local reset CLI so
/// they always agree on the file.
pub fn admin_path_for_token(token_path: &Path) -> PathBuf {
    token_path
        .parent()
        .map(|p| p.join("dashboard-admin.json"))
        .unwrap_or_else(|| PathBuf::from("dashboard-admin.json"))
}

/// Hash `password` (Argon2id) + atomically write the admin record at
/// `path`, restricting it to the current user. Shared by first-run setup
/// and the local reset path so the on-disk format is identical.
fn write_admin_record(path: &Path, username: &str, password: &str) -> Result<AdminRecord, String> {
    let salt = SaltString::generate(&mut OsRng);
    let hash = Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| format!("hash: {e}"))?
        .to_string();
    let rec = AdminRecord {
        username: username.to_string(),
        hash,
        created_at: now_secs(),
    };
    let body = serde_json::to_vec_pretty(&rec).map_err(|e| format!("encode: {e}"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, &body).map_err(|e| format!("write: {e}"))?;
    let _ = crate::os_secure::restrict_to_current_user(&tmp);
    std::fs::rename(&tmp, path).map_err(|e| format!("rename: {e}"))?;
    let _ = crate::os_secure::restrict_to_current_user(path);
    Ok(rec)
}

/// The current admin username at `admin_path`, or `None` if no admin exists
/// yet. Never returns the password hash — callers that reset reuse the
/// existing username without ever seeing the secret.
pub fn read_admin_username(admin_path: &Path) -> Option<String> {
    let bytes = std::fs::read(admin_path).ok()?;
    serde_json::from_slice::<AdminRecord>(&bytes)
        .ok()
        .map(|r| r.username)
}

/// **Local operator recovery only.** Overwrite the dashboard admin
/// credential at `admin_path` with a new username + a freshly Argon2id-
/// hashed password, using the SAME storage format as first-run setup.
///
/// There is deliberately NO network path to this — it is a CLI / filesystem
/// operation an operator runs locally (it requires write access to the
/// admin file). It does NOT print or read the existing password/hash, does
/// NOT weaken session auth, and does NOT touch any other state. Existing
/// in-memory sessions are not invalidated here; restart the bridge to drop
/// them (a restart also reloads this new credential).
pub fn reset_admin_credential(
    admin_path: &Path,
    username: &str,
    password: &str,
) -> Result<(), String> {
    let username = username.trim();
    if username.is_empty() {
        return Err("username required".to_string());
    }
    if password.len() < MIN_PASSWORD_LEN {
        return Err(format!("password too short (min {MIN_PASSWORD_LEN} chars)"));
    }
    write_admin_record(admin_path, username, password).map(|_| ())
}

// ── Sessions (in-memory) ────────────────────────────────────────

struct Session {
    username: String,
    expires_at: i64,
}

/// In-memory session table, keyed by a random opaque session id.
#[derive(Clone)]
struct SessionStore {
    inner: Arc<RwLock<HashMap<String, Session>>>,
}

impl SessionStore {
    fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    fn create(&self, username: &str) -> String {
        let mut buf = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut buf);
        let sid = hex::encode(buf);
        if let Ok(mut m) = self.inner.write() {
            m.insert(
                sid.clone(),
                Session {
                    username: username.to_string(),
                    expires_at: now_secs() + SESSION_TTL_SECS,
                },
            );
        }
        sid
    }

    /// Return the session's username if it exists and has not expired.
    /// Prunes the entry when expired.
    fn validate(&self, sid: &str) -> Option<String> {
        let now = now_secs();
        // Fast path: read lock.
        if let Ok(m) = self.inner.read() {
            match m.get(sid) {
                Some(s) if s.expires_at > now => return Some(s.username.clone()),
                Some(_) => {} // expired → fall through to prune
                None => return None,
            }
        }
        // Expired: remove under a write lock.
        if let Ok(mut m) = self.inner.write() {
            m.remove(sid);
        }
        None
    }

    fn remove(&self, sid: &str) {
        if let Ok(mut m) = self.inner.write() {
            m.remove(sid);
        }
    }
}

// ── Combined handle stored on AppState ──────────────────────────

/// Dashboard auth state: the durable admin credential + the in-memory
/// session table. Cloned cheaply (Arc inside).
#[derive(Clone)]
pub struct DashboardAuth {
    admin: AdminStore,
    sessions: SessionStore,
}

impl DashboardAuth {
    /// Build from the bridge-token path: the admin record lives in the
    /// same directory (`dashboard-admin.json`) so it sits with the
    /// operator's other Relix state.
    pub fn from_token_path(token_path: &Path) -> Self {
        let admin_path = admin_path_for_token(token_path);
        Self {
            admin: AdminStore::load(&admin_path),
            sessions: SessionStore::new(),
        }
    }

    /// Validate a raw session-cookie value. Used by the auth middleware
    /// to admit a logged-in dashboard request. Returns the username.
    pub fn validate_session(&self, sid: &str) -> Option<String> {
        self.sessions.validate(sid)
    }
}

// ── Cookie helpers ──────────────────────────────────────────────

/// Pull the `relix_session` value out of the request `Cookie` header.
pub fn session_cookie_value(req: &Request) -> Option<String> {
    session_cookie_from_headers(req.headers())
}

/// Pull the `relix_session` value out of a raw header map — the
/// header-only variant used by handlers that also consume the body
/// (`Json` extractor) and so cannot take the whole `Request`.
pub fn session_cookie_from_headers(headers: &header::HeaderMap) -> Option<String> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    for pair in raw.split(';') {
        let pair = pair.trim();
        if let Some(v) = pair.strip_prefix(&format!("{SESSION_COOKIE}=")) {
            let v = v.trim();
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

fn set_session_cookie(sid: &str) -> String {
    // HttpOnly so JS cannot read it; SameSite=Strict so a cross-site
    // form/link cannot ride it; Path=/ for the whole app. No `Secure`
    // because the operator console runs over loopback http — a reverse
    // proxy terminating TLS can re-add it.
    format!("{SESSION_COOKIE}={sid}; HttpOnly; SameSite=Strict; Path=/; Max-Age={SESSION_TTL_SECS}")
}

fn clear_session_cookie() -> String {
    format!("{SESSION_COOKIE}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0")
}

// ── Request/response bodies ─────────────────────────────────────

#[derive(Deserialize)]
pub struct Credentials {
    username: String,
    password: String,
}

#[derive(Serialize)]
struct StatusBody {
    needs_setup: bool,
    authenticated: bool,
    username: Option<String>,
}

#[derive(Serialize)]
struct MeBody {
    username: String,
}

#[derive(Serialize)]
struct ErrBody {
    error: &'static str,
}

fn json_err(status: StatusCode, error: &'static str) -> Response {
    (status, Json(ErrBody { error })).into_response()
}

/// Attach a `Set-Cookie` header to a JSON 200 response.
fn ok_with_cookie<T: Serialize>(body: T, cookie: String) -> Response {
    let mut resp = (StatusCode::OK, Json(body)).into_response();
    if let Ok(hv) = header::HeaderValue::from_str(&cookie) {
        resp.headers_mut().append(header::SET_COOKIE, hv);
    }
    resp
}

// ── Handlers ────────────────────────────────────────────────────

/// `GET /v1/auth/status` — public. Tells the dashboard whether to show
/// the first-run setup form, the login form, or the app.
pub async fn status(State(state): State<AppState>, req: Request) -> Response {
    let auth = &state.dashboard_auth;
    let username = session_cookie_value(&req).and_then(|sid| auth.validate_session(&sid));
    let body = StatusBody {
        needs_setup: !auth.admin.exists(),
        authenticated: username.is_some(),
        username,
    };
    (StatusCode::OK, Json(body)).into_response()
}

/// `POST /v1/auth/setup` — first-run only. Creates the admin account and
/// logs in. Refuses once an admin already exists (use login instead).
pub async fn setup(State(state): State<AppState>, Json(creds): Json<Credentials>) -> Response {
    let auth = &state.dashboard_auth;
    if auth.admin.exists() {
        return json_err(
            StatusCode::CONFLICT,
            "admin already configured — log in instead",
        );
    }
    let username = creds.username.trim();
    if username.is_empty() {
        return json_err(StatusCode::BAD_REQUEST, "username required");
    }
    if creds.password.len() < MIN_PASSWORD_LEN {
        return json_err(StatusCode::BAD_REQUEST, "password too short (min 8 chars)");
    }
    if let Err(e) = auth.admin.create(username, &creds.password) {
        tracing::warn!(error = %e, "dashboard auth: admin create failed");
        return json_err(StatusCode::INTERNAL_SERVER_ERROR, "could not create admin");
    }
    let sid = auth.sessions.create(username);
    ok_with_cookie(
        MeBody {
            username: username.to_string(),
        },
        set_session_cookie(&sid),
    )
}

/// `POST /v1/auth/login` — verify the admin password and mint a session.
pub async fn login(State(state): State<AppState>, Json(creds): Json<Credentials>) -> Response {
    let auth = &state.dashboard_auth;
    if !auth.admin.exists() {
        return json_err(
            StatusCode::CONFLICT,
            "no admin configured — run setup first",
        );
    }
    match auth.admin.verify(creds.username.trim(), &creds.password) {
        Some(username) => {
            let sid = auth.sessions.create(&username);
            ok_with_cookie(MeBody { username }, set_session_cookie(&sid))
        }
        None => json_err(StatusCode::UNAUTHORIZED, "invalid username or password"),
    }
}

/// `POST /v1/auth/logout` — drop the session and clear the cookie.
pub async fn logout(State(state): State<AppState>, req: Request) -> Response {
    if let Some(sid) = session_cookie_value(&req) {
        state.dashboard_auth.sessions.remove(&sid);
    }
    let mut resp = (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response();
    if let Ok(hv) = header::HeaderValue::from_str(&clear_session_cookie()) {
        resp.headers_mut().append(header::SET_COOKIE, hv);
    }
    resp
}

/// `GET /v1/auth/me` — the logged-in username, or 401.
pub async fn me(State(state): State<AppState>, req: Request) -> Response {
    match session_cookie_value(&req).and_then(|sid| state.dashboard_auth.validate_session(&sid)) {
        Some(username) => (StatusCode::OK, Json(MeBody { username })).into_response(),
        None => json_err(StatusCode::UNAUTHORIZED, "not logged in"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (DashboardAuth, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let token = tmp.path().join("bridge-token");
        (DashboardAuth::from_token_path(&token), tmp)
    }

    #[test]
    fn admin_setup_then_verify_roundtrips() {
        let (auth, _tmp) = store();
        assert!(!auth.admin.exists());
        auth.admin.create("ops", "hunter2pass").unwrap();
        assert!(auth.admin.exists());
        assert_eq!(auth.admin.username().as_deref(), Some("ops"));
        // Correct password verifies; wrong does not.
        assert_eq!(
            auth.admin.verify("ops", "hunter2pass").as_deref(),
            Some("ops")
        );
        assert!(auth.admin.verify("ops", "wrong").is_none());
        assert!(auth.admin.verify("other", "hunter2pass").is_none());
    }

    #[test]
    fn admin_record_persists_across_reload() {
        let tmp = tempfile::tempdir().unwrap();
        let token = tmp.path().join("bridge-token");
        let a1 = DashboardAuth::from_token_path(&token);
        a1.admin.create("ops", "hunter2pass").unwrap();
        // A fresh handle on the same path reads the persisted admin.
        let a2 = DashboardAuth::from_token_path(&token);
        assert!(a2.admin.exists());
        assert_eq!(
            a2.admin.verify("ops", "hunter2pass").as_deref(),
            Some("ops")
        );
    }

    #[test]
    fn session_create_validate_remove() {
        let (auth, _tmp) = store();
        let sid = auth.sessions.create("ops");
        assert_eq!(auth.validate_session(&sid).as_deref(), Some("ops"));
        auth.sessions.remove(&sid);
        assert!(auth.validate_session(&sid).is_none());
        // Unknown session id is rejected.
        assert!(auth.validate_session("deadbeef").is_none());
    }

    #[test]
    fn stored_hash_is_argon2id_phc_not_plaintext() {
        let (auth, _tmp) = store();
        auth.admin.create("ops", "hunter2pass").unwrap();
        let rec = auth.admin.cached.read().unwrap().clone().unwrap();
        assert!(rec.hash.starts_with("$argon2id$"), "got: {}", rec.hash);
        assert!(!rec.hash.contains("hunter2pass"));
    }

    #[test]
    fn admin_path_is_next_to_the_token() {
        let p = admin_path_for_token(Path::new("/x/y/bridge-token"));
        assert!(p.ends_with("dashboard-admin.json"));
        assert_eq!(p.parent().unwrap(), Path::new("/x/y"));
    }

    #[test]
    fn reset_changes_password_old_fails_new_works() {
        let tmp = tempfile::tempdir().unwrap();
        let token = tmp.path().join("bridge-token");
        let admin = admin_path_for_token(&token);
        // First-run setup, then verify the old password.
        let a1 = DashboardAuth::from_token_path(&token);
        a1.admin.create("ops", "oldpassword").unwrap();
        assert_eq!(
            a1.admin.verify("ops", "oldpassword").as_deref(),
            Some("ops")
        );
        // Reset keeps the username (read from disk) but sets a new password.
        let user = read_admin_username(&admin).unwrap();
        assert_eq!(user, "ops");
        reset_admin_credential(&admin, &user, "newpassword1").unwrap();
        // A FRESH handle (simulating a bridge restart) honors ONLY the new
        // password — the old one is gone.
        let a2 = DashboardAuth::from_token_path(&token);
        assert_eq!(
            a2.admin.verify("ops", "newpassword1").as_deref(),
            Some("ops")
        );
        assert!(
            a2.admin.verify("ops", "oldpassword").is_none(),
            "old password must stop working after reset"
        );
    }

    #[test]
    fn reset_can_set_username_and_creates_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let token = tmp.path().join("bridge-token");
        let admin = admin_path_for_token(&token);
        // No admin yet → reset CREATES it with the given username.
        assert!(read_admin_username(&admin).is_none());
        reset_admin_credential(&admin, "newadmin", "secretpass1").unwrap();
        assert_eq!(read_admin_username(&admin).as_deref(), Some("newadmin"));
        let a = DashboardAuth::from_token_path(&token);
        assert_eq!(
            a.admin.verify("newadmin", "secretpass1").as_deref(),
            Some("newadmin")
        );
    }

    #[test]
    fn reset_validates_and_never_stores_plaintext() {
        let tmp = tempfile::tempdir().unwrap();
        let admin = tmp.path().join("dashboard-admin.json");
        // Empty username + short password are refused.
        assert!(reset_admin_credential(&admin, "  ", "longenough").is_err());
        assert!(reset_admin_credential(&admin, "ops", "short").is_err());
        // A valid reset stores an Argon2id PHC hash, never the plaintext.
        reset_admin_credential(&admin, "ops", "validpassword").unwrap();
        let raw = std::fs::read_to_string(&admin).unwrap();
        assert!(raw.contains("$argon2id$"), "got: {raw}");
        assert!(
            !raw.contains("validpassword"),
            "password must not be stored in plaintext"
        );
    }

    #[test]
    fn cookie_value_parses_from_header() {
        let req = Request::builder()
            .header("cookie", "foo=bar; relix_session=abc123; baz=1")
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(session_cookie_value(&req).as_deref(), Some("abc123"));
        let req = Request::builder().body(axum::body::Body::empty()).unwrap();
        assert!(session_cookie_value(&req).is_none());
    }

    #[test]
    fn session_cookie_from_headers_matches_request_variant() {
        // The header-only variant (used by the company-init handler that
        // also consumes a JSON body) parses the same cookie.
        let mut headers = header::HeaderMap::new();
        headers.insert(
            header::COOKIE,
            header::HeaderValue::from_static("x=1; relix_session=sess-xyz; y=2"),
        );
        assert_eq!(
            session_cookie_from_headers(&headers).as_deref(),
            Some("sess-xyz")
        );
        // No cookie header → None (an unauthenticated caller is rejected).
        let empty = header::HeaderMap::new();
        assert!(session_cookie_from_headers(&empty).is_none());
    }
}
