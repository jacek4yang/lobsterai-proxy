//! Account pool: directory discovery + seeding, credits-based selection,
//! sticky session binding, health cooldowns, and (account, model)-scoped 429
//! cooling. Selection prefers the healthy account with the highest known
//! remaining credits (falls back to round-robin when credits are unknown).

use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use super::credential::{
    cleanup_stale_temp_files, read_credential_file, Credential, CredentialError,
};
use crate::config::LimitConfig;

/// A credential's health state shared by the pool.
pub struct PoolEntry {
    pub credential: Arc<Credential>,
}

/// Why an account is cooling down.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CoolKind {
    /// Out of credits → long cooldown.
    HardCredit,
    /// 429 → short cooldown (optionally quota-reset parsed).
    SoftRate,
    /// Consecutive unexpected errors → medium cooldown.
    ErrThreshold,
}

pub struct Pool {
    entries: RwLock<Vec<PoolEntry>>,
    server_secret: Vec<u8>,
    limits: LimitConfig,
    state: Mutex<PoolState>,
}

#[derive(Default)]
struct PoolState {
    /// Remaining credits per account id (learned from profile-summary).
    credits: HashMap<String, i64>,
    /// Cooldowns by kind: (id, kind) → until.
    fail_until: HashMap<(String, CoolKind), Instant>,
    /// (account id, model) → cooldown until (429 quota resets).
    model_fail: HashMap<(String, String), Instant>,
    /// session fingerprint → (account id, last touch).
    sticky: HashMap<String, (String, Instant)>,
    /// FIFO eviction order for the sticky map.
    sticky_order: VecDeque<String>,
    /// Round-robin counter within the equal-credits rank.
    rr: usize,
    /// Consecutive non-429/non-credit error counts per account.
    err_counts: HashMap<String, usize>,
}

impl Pool {
    pub fn new(server_secret: Vec<u8>, limits: LimitConfig) -> Self {
        Self {
            entries: RwLock::new(Vec::new()),
            server_secret,
            limits,
            state: Mutex::new(PoolState::default()),
        }
    }

    pub fn server_secret(&self) -> &[u8] {
        &self.server_secret
    }

    pub fn len(&self) -> usize {
        self.entries.read().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn credential_by_safe_name(&self, safe_name: &str) -> Option<Arc<Credential>> {
        self.entries
            .read()
            .unwrap()
            .iter()
            .map(|e| e.credential.clone())
            .find(|c| c.safe_name == safe_name)
    }

    /// Load all `lobsterai-*.json` files from the managed dir, dedup by uid.
    ///
    /// Also sweeps crash leftovers from interrupted atomic writes.
    pub fn load_dir(&self, dir: &Path) {
        let swept = cleanup_stale_temp_files(dir);
        if swept > 0 {
            tracing::info!(swept, "swept stale credential temp files");
        }
        let files = list_credential_files(dir);
        self.load_files(&files);
    }

    /// Load credential files, ignoring duplicates (same path or same uid).
    pub fn load_files(&self, paths: &[PathBuf]) {
        let mut entries = self.entries.write().unwrap();
        let mut known_uids: std::collections::HashSet<String> =
            entries.iter().map(|e| e.credential.uid()).collect();
        let mut known_paths: std::collections::HashSet<PathBuf> =
            entries.iter().map(|e| e.credential.path.clone()).collect();
        let mut loaded = 0usize;
        for path in paths {
            if !path.is_file() || known_paths.contains(path) {
                continue;
            }
            let data = match read_credential_file(path) {
                Ok(data) => data,
                Err(CredentialError::Io { .. }) => continue,
                Err(err) => {
                    tracing::warn!(error = %err, "credential file rejected");
                    continue;
                }
            };
            if known_uids.contains(&data.uid) {
                tracing::warn!("duplicate uid ignored");
                continue;
            }
            known_uids.insert(data.uid.clone());
            known_paths.insert(path.clone());
            let credential = Credential::new(path.clone(), data, &self.server_secret);
            tracing::info!(credential = %credential.safe_name, "account loaded");
            entries.push(PoolEntry { credential });
            loaded += 1;
        }
        drop(entries);
        if loaded > 0 {
            tracing::info!(loaded, total = self.len(), "account pool updated");
        }
    }

    /// Drop entries whose file disappeared.
    pub fn prune(&self) {
        let mut entries = self.entries.write().unwrap();
        let before = entries.len();
        entries.retain(|e| e.credential.path.is_file());
        let removed = entries.len() != before;
        let ids: std::collections::HashSet<String> =
            entries.iter().map(|e| e.credential.uid()).collect();
        drop(entries);
        if removed {
            let mut state = self.state.lock().unwrap();
            state.credits.retain(|id, _| ids.contains(id));
            state.fail_until.retain(|(id, _), _| ids.contains(id));
            state.model_fail.retain(|(id, _), _| ids.contains(id));
            state.err_counts.retain(|id, _| ids.contains(id));
            let sticky_ids: Vec<String> = state
                .sticky
                .iter()
                .filter(|(_, (id, _))| !ids.contains(id))
                .map(|(k, _)| k.clone())
                .collect();
            for key in sticky_ids {
                remove_sticky(&mut state, &key);
            }
        }
    }

    fn globally_healthy(&self, id: &str, state: &PoolState) -> bool {
        state
            .fail_until
            .iter()
            .all(|((owner, _), until)| owner != id || Instant::now() >= *until)
    }
    fn model_healthy(&self, id: &str, model: &str, state: &PoolState) -> bool {
        state
            .model_fail
            .get(&(id.to_owned(), model.to_owned()))
            .is_none_or(|until| Instant::now() >= *until)
    }

    fn evict_sticky(&self, state: &mut PoolState) {
        let now = Instant::now();
        let ttl = Duration::from_secs(self.limits.sticky_ttl_secs);
        while let Some(key) = state.sticky_order.front().cloned() {
            let expired = match state.sticky.get(&key) {
                Some((_, touched)) => now.duration_since(*touched) > ttl,
                None => true,
            };
            if expired || state.sticky.len() > self.limits.sticky_max {
                remove_sticky(state, &key);
            } else {
                break;
            }
        }
    }

    /// Pick an account for this request. Sticky sessions keep their account
    /// while it stays healthy; otherwise the healthy account with the highest
    /// known remaining credits wins (round-robin within the top rank).
    pub fn pick(&self, session_fp: Option<&str>, model: &str) -> Option<Arc<Credential>> {
        {
            let mut entries = self.entries.write().unwrap();
            entries.retain(|e| e.credential.path.is_file());
        }
        let mut state = self.state.lock().unwrap();
        self.evict_sticky(&mut state);
        if let Some(fp) = session_fp {
            if let Some((id, _)) = state.sticky.get(fp).cloned() {
                let credential = {
                    let entries = self.entries.read().unwrap();
                    entries
                        .iter()
                        .map(|e| e.credential.clone())
                        .find(|c| c.uid() == id)
                };
                if let Some(credential) = credential {
                    if self.globally_healthy(&id, &state) && self.model_healthy(&id, model, &state)
                    {
                        state
                            .sticky
                            .insert(fp.to_owned(), (id.clone(), Instant::now()));
                        move_sticky_to_back(&mut state, fp);
                        return Some(credential);
                    }
                }
                remove_sticky(&mut state, fp);
            }
        }
        let entries = self.entries.read().unwrap();
        if entries.is_empty() {
            return None;
        }
        let candidates: Vec<Arc<Credential>> = entries
            .iter()
            .map(|e| e.credential.clone())
            .filter(|c| {
                let id = c.uid();
                self.globally_healthy(&id, &state) && self.model_healthy(&id, model, &state)
            })
            .collect();
        if candidates.is_empty() {
            return None;
        }
        // Rank: highest known credits first; unknown credits (-1) last.
        let rank = |credential: &Arc<Credential>| -> i64 {
            state.credits.get(&credential.uid()).copied().unwrap_or(-1)
        };
        let mut ranked: Vec<(i64, Arc<Credential>)> =
            candidates.into_iter().map(|c| (rank(&c), c)).collect();
        ranked.sort_by_key(|a| std::cmp::Reverse(a.0));
        let top_credits = ranked[0].0;
        let top: Vec<Arc<Credential>> = ranked
            .into_iter()
            .filter(|(credits, _)| *credits == top_credits)
            .map(|(_, c)| c)
            .collect();
        let credential = top[state.rr % top.len()].clone();
        state.rr = state.rr.wrapping_add(1);
        drop(entries);
        if let Some(fp) = session_fp {
            bind_sticky(&mut state, fp, &credential.uid(), self.limits.sticky_max);
        }
        Some(credential)
    }

    /// Record learned remaining credits for an account (check-in/credit loop).
    pub fn set_credits(&self, id: &str, credits: i64) {
        self.state
            .lock()
            .unwrap()
            .credits
            .insert(id.to_owned(), credits);
    }

    /// Remaining credits for an account, if learned.
    pub fn credits_of(&self, id: &str) -> Option<i64> {
        self.state.lock().unwrap().credits.get(id).copied()
    }

    /// Apply an upstream status to pool health.
    /// 401/403 cool the whole account; 429 cools (account, model).
    pub fn note_status(
        &self,
        credential: &Arc<Credential>,
        status: u16,
        model: &str,
        _raw_body: &[u8],
    ) {
        match status {
            401 | 403 => {
                let until = Instant::now() + Duration::from_secs(self.limits.cred_cooldown_secs);
                self.state
                    .lock()
                    .unwrap()
                    .fail_until
                    .insert((credential.uid(), CoolKind::ErrThreshold), until);
                tracing::warn!(credential = %credential.safe_name, status, cooldown_secs = self.limits.cred_cooldown_secs, "account cooled down");
            }
            429 => {
                let cooldown_secs = self.limits.model_cooldown_secs;
                let mut state = self.state.lock().unwrap();
                state.model_fail.insert(
                    (credential.uid(), model.to_owned()),
                    Instant::now() + Duration::from_secs(cooldown_secs.max(1)),
                );
                // Unbind sticky sessions pinned to this account for the model.
                let affected: Vec<String> = state
                    .sticky
                    .iter()
                    .filter(|(_, (id, _))| *id == credential.uid())
                    .map(|(k, _)| k.clone())
                    .collect();
                for key in affected {
                    remove_sticky(&mut state, &key);
                }
                drop(state);
                tracing::warn!(credential = %credential.safe_name, model, cooldown_secs, "model cooled down after 429");
            }
            _ => {}
        }
    }

    /// Out-of-credits: long cooldown so the account rotates out until the
    /// next check-in grants credits.
    pub fn note_hard_credit(&self, credential: &Arc<Credential>) {
        let until = Instant::now() + Duration::from_secs(self.limits.hard_credit_cooldown_secs);
        self.state
            .lock()
            .unwrap()
            .fail_until
            .insert((credential.uid(), CoolKind::HardCredit), until);
        tracing::warn!(
            credential = %credential.safe_name,
            cooldown_secs = self.limits.hard_credit_cooldown_secs,
            "account out of credits; long cooldown"
        );
    }

    /// Record one unexpected error; at the threshold the account cools down.
    pub fn note_error_threshold(&self, credential: &Arc<Credential>) {
        let mut state = self.state.lock().unwrap();
        let count = state.err_counts.entry(credential.uid()).or_insert(0);
        *count += 1;
        if *count >= 3 {
            let until = Instant::now() + Duration::from_secs(self.limits.cred_cooldown_secs);
            state
                .fail_until
                .insert((credential.uid(), CoolKind::ErrThreshold), until);
            state.err_counts.insert(credential.uid(), 0);
            tracing::warn!(credential = %credential.safe_name, "consecutive errors; account cooled down");
        }
    }

    /// A successful request resets the consecutive-error counter.
    pub fn note_success(&self, credential: &Arc<Credential>) {
        self.state
            .lock()
            .unwrap()
            .err_counts
            .insert(credential.uid(), 0);
    }

    /// Earliest model-cooldown reset across healthy accounts, if all of them
    /// are cooling (surfaces a retry time in 429 responses).
    pub fn all_cooled_until(&self, model: &str) -> Option<i64> {
        let state = self.state.lock().unwrap();
        let entries = self.entries.read().unwrap();
        let now = Instant::now();
        let healthy: Vec<&PoolEntry> = entries
            .iter()
            .filter(|e| self.globally_healthy(&e.credential.uid(), &state))
            .collect();
        if healthy.is_empty() {
            return None;
        }
        let untils: Vec<i64> = healthy
            .iter()
            .filter_map(|e| {
                state
                    .model_fail
                    .get(&(e.credential.uid(), model.to_owned()))
            })
            .map(|until| {
                super::credential::now_secs()
                    + until.saturating_duration_since(now).as_secs() as i64
            })
            .collect();
        if untils.len() < healthy.len() {
            return None;
        }
        untils.iter().min().copied()
    }

    /// Safe snapshot for /admin/status: names, health, cooldowns; no secrets.
    pub fn snapshot(&self, model: &str) -> Vec<Value> {
        let state = self.state.lock().unwrap();
        let entries = self.entries.read().unwrap();
        let now = Instant::now();
        entries
            .iter()
            .map(|e| {
                let credential = &e.credential;
                let id = credential.uid();
                let mut entry = serde_json::json!({
                    "name": credential.safe_name,
                    "file": credential.safe_name,
                    "healthy": self.globally_healthy(&id, &state),
                    "sticky_sessions": state
                        .sticky
                        .values()
                        .filter(|(owner, _)| *owner == id)
                        .count(),
                });
                if let Some(credits) = state.credits.get(&id) {
                    entry["credits"] = serde_json::json!(credits);
                }
                for (kind, key) in [
                    (CoolKind::HardCredit, "hard_credit_cooldown_secs_left"),
                    (CoolKind::ErrThreshold, "cooldown_secs_left"),
                ] {
                    if let Some(until) = state.fail_until.get(&(id.clone(), kind)) {
                        if *until > now {
                            entry[key] =
                                serde_json::json!(until.saturating_duration_since(now).as_secs());
                        }
                    }
                }
                let model_until = state.model_fail.get(&(id.clone(), model.to_owned()));
                if let Some(until) = model_until {
                    if *until > now {
                        entry["model_cooldown_secs_left"] =
                            serde_json::json!(until.saturating_duration_since(now).as_secs());
                    }
                }
                entry["token_expires_at_secs"] = serde_json::json!(credential.expires_at_secs());
                entry["last_refresh_secs"] = serde_json::json!(credential.last_refresh_secs());
                entry
            })
            .collect()
    }

    /// Refresh accounts that are near expiry or due for daily keepalive.
    pub async fn refresh_due(
        &self,
        http: &reqwest::Client,
        base_url: &str,
        margin_secs: i64,
        keepalive_secs: u64,
        metrics: Option<&crate::observability::Metrics>,
    ) {
        let entries: Vec<Arc<Credential>> = self
            .entries
            .read()
            .unwrap()
            .iter()
            .map(|e| e.credential.clone())
            .collect();
        let now_s = super::credential::now_secs();
        for credential in entries {
            let expires_at = credential.expires_at_secs();
            let last = credential.last_refresh_secs();
            let expiry_due = expires_at == 0 || credential.needs_refresh(margin_secs, now_s);
            let keepalive_due = keepalive_secs > 0
                && last > 0
                && now_s - last >= keepalive_secs as i64
                && !expiry_due;
            if !expiry_due && !keepalive_due {
                continue;
            }
            match super::refresh::ensure_fresh(&credential, http, base_url, margin_secs).await {
                Ok(_) => {
                    if let Some(metrics) = metrics {
                        metrics.record_refresh(true);
                    }
                    tracing::info!(credential = %credential.safe_name, reason = if expiry_due { "expiry" } else { "keepalive" }, "proactive refresh ok")
                }
                Err(err) => {
                    if let Some(metrics) = metrics {
                        metrics.record_refresh(false);
                    }
                    tracing::warn!(credential = %credential.safe_name, error = %err, "proactive refresh failed");
                    self.note_status(&credential, 401, "", b"");
                }
            }
        }
    }
}

fn bind_sticky(state: &mut PoolState, fp: &str, id: &str, max: usize) {
    if !state.sticky.contains_key(fp) && state.sticky.len() >= max {
        if let Some(oldest) = state.sticky_order.front().cloned() {
            remove_sticky(state, &oldest);
        }
    }
    state
        .sticky
        .insert(fp.to_owned(), (id.to_owned(), Instant::now()));
    state.sticky_order.push_back(fp.to_owned());
}

fn move_sticky_to_back(state: &mut PoolState, fp: &str) {
    state.sticky_order.retain(|k| k != fp);
    state.sticky_order.push_back(fp.to_owned());
}

fn remove_sticky(state: &mut PoolState, fp: &str) {
    state.sticky.remove(fp);
    state.sticky_order.retain(|k| k != fp);
}

/// Sorted list of `lobsterai-*.json` files directly inside `dir`.
pub fn list_credential_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = read_dir
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|name| name.starts_with("lobsterai-") && name.ends_with(".json"))
        })
        .collect();
    files.sort();
    files
}

/// Seed credentials: copy missing `lobsterai-*.json` files from read-only
/// source dirs into the managed dir (never the other way around).
pub fn seed_from_dirs(managed_dir: &Path, sources: &[PathBuf]) {
    if std::fs::create_dir_all(managed_dir).is_err() {
        return;
    }
    let have: std::collections::HashSet<String> = list_credential_files(managed_dir)
        .iter()
        .filter_map(|p| read_credential_file(p).ok().map(|d| d.uid))
        .collect();
    for source in sources {
        for file in list_credential_files(source) {
            let Ok(data) = read_credential_file(&file) else {
                continue;
            };
            if have.contains(&data.uid) {
                continue;
            }
            let dest = managed_dir.join(file.file_name().unwrap_or_default());
            if dest.exists() {
                continue;
            }
            match std::fs::copy(&file, &dest) {
                Ok(_) => {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let _ =
                            std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o600));
                    }
                    tracing::info!("seeded credential");
                }
                Err(err) => {
                    tracing::warn!(error = %err, "seed copy failed")
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_credential(dir: &Path, uid: &str) -> PathBuf {
        let value = json!({
            "account": {"uid": uid, "nickname": "n"},
            "auth": {
                "accessToken": format!("tok-{uid}"),
                "refreshToken": "rt",
                "expiresAt": 4_000_000_000i64
            }
        });
        let path = dir.join(format!("lobsterai-{uid}.json"));
        std::fs::write(&path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
        path
    }

    fn tempdir(label: &str) -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let id = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let path =
            std::env::temp_dir().join(format!("lap-pool-{label}-{}-{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        let _ = std::fs::create_dir_all(&path);
        path
    }

    #[test]
    fn picks_credentials_and_binds_sticky() {
        let dir = tempdir("bind");
        let a = make_credential(&dir, "u-a");
        let b = make_credential(&dir, "u-b");
        let pool = Pool::new(
            b"s".to_vec(),
            LimitConfig {
                sticky_ttl_secs: 60,
                ..Default::default()
            },
        );
        pool.load_files(&[a, b]);
        assert_eq!(pool.len(), 2);
        let first = pool.pick(Some("sess"), "deepseek-flash").unwrap();
        let second = pool.pick(Some("sess"), "deepseek-flash").unwrap();
        assert_eq!(
            first.uid(),
            second.uid(),
            "sticky binding keeps the account"
        );
        let other = pool.pick(Some("other"), "deepseek-flash").unwrap();
        let _ = other;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn highest_credits_win_when_known() {
        let dir = tempdir("credits");
        let a = make_credential(&dir, "u-a");
        let b = make_credential(&dir, "u-b");
        let pool = Pool::new(b"s".to_vec(), LimitConfig::default());
        pool.load_files(&[a, b]);
        pool.set_credits("u-a", 100);
        pool.set_credits("u-b", 5000);
        for _ in 0..5 {
            let picked = pool.pick(None, "m").unwrap();
            assert_eq!(picked.uid(), "u-b", "higher credits win");
        }
        // After credits equalize, round-robin alternates.
        pool.set_credits("u-a", 5000);
        let first = pool.pick(None, "m").unwrap().uid();
        let second = pool.pick(None, "m").unwrap().uid();
        assert_ne!(first, second, "equal credits round-robin");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn model_429_cooling_skips_account() {
        let dir = tempdir("cool");
        let a = make_credential(&dir, "u-a");
        let pool = Pool::new(b"s".to_vec(), LimitConfig::default());
        pool.load_files(&[a]);
        let cred = pool.pick(None, "m").unwrap();
        pool.note_status(&cred, 429, "m", b"");
        assert!(
            pool.pick(None, "m").is_none(),
            "all accounts cooling for model"
        );
        assert!(pool.pick(None, "other").is_some(), "other model unaffected");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn auth_failure_cools_whole_account() {
        let dir = tempdir("auth-cool");
        let a = make_credential(&dir, "u-a");
        let pool = Pool::new(b"s".to_vec(), LimitConfig::default());
        pool.load_files(&[a]);
        let cred = pool.pick(None, "m").unwrap();
        pool.note_status(&cred, 401, "m", b"");
        assert!(pool.pick(None, "m").is_none());
        assert!(
            pool.pick(None, "other").is_none(),
            "global cooldown affects all models"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hard_credit_cooling_expires_after_configured_time() {
        let dir = tempdir("hard");
        let a = make_credential(&dir, "u-a");
        let pool = Pool::new(
            b"s".to_vec(),
            LimitConfig {
                hard_credit_cooldown_secs: 1,
                ..Default::default()
            },
        );
        pool.load_files(&[a]);
        let cred = pool.pick(None, "m").unwrap();
        pool.note_hard_credit(&cred);
        assert!(pool.pick(None, "m").is_none());
        std::thread::sleep(Duration::from_millis(1100));
        assert!(pool.pick(None, "m").is_some(), "cooldown expired");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn error_threshold_cools_after_three() {
        let dir = tempdir("errs");
        let a = make_credential(&dir, "u-a");
        let pool = Pool::new(b"s".to_vec(), LimitConfig::default());
        pool.load_files(&[a]);
        let cred = pool.pick(None, "m").unwrap();
        pool.note_error_threshold(&cred);
        pool.note_error_threshold(&cred);
        assert!(pool.pick(None, "m").is_some(), "below threshold");
        pool.note_error_threshold(&cred);
        assert!(pool.pick(None, "m").is_none(), "threshold reached");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn success_resets_error_counter() {
        let dir = tempdir("reset");
        let a = make_credential(&dir, "u-a");
        let pool = Pool::new(b"s".to_vec(), LimitConfig::default());
        pool.load_files(&[a]);
        let cred = pool.pick(None, "m").unwrap();
        pool.note_error_threshold(&cred);
        pool.note_error_threshold(&cred);
        pool.note_success(&cred);
        pool.note_error_threshold(&cred);
        pool.note_error_threshold(&cred);
        assert!(pool.pick(None, "m").is_some(), "counter reset by success");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn seeding_copies_once_and_respects_uid_dedup() {
        let source = tempdir("seed-src");
        let managed = tempdir("seed-dst");
        make_credential(&source, "u-seed");
        seed_from_dirs(&managed, std::slice::from_ref(&source));
        seed_from_dirs(&managed, std::slice::from_ref(&source));
        assert_eq!(list_credential_files(&managed).len(), 1);
        let _ = std::fs::remove_dir_all(&source);
        let _ = std::fs::remove_dir_all(&managed);
    }

    #[test]
    fn public_snapshot_does_not_expose_credential_filenames() {
        let dir = tempdir("private-snapshot");
        make_credential(&dir, "synthetic-private-id");
        let pool = Pool::new(b"s".to_vec(), LimitConfig::default());
        pool.load_dir(&dir);
        let snapshot = serde_json::to_string(&pool.snapshot("m")).unwrap();
        assert!(!snapshot.contains("synthetic-private-id"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
