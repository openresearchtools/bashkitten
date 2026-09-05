use crate::config::{atomic_private_json, atomic_private_json_create};
use crate::paths::{AppPaths, set_private_file};
use anyhow::{Context, Result, bail};
use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::path::Path;

const SESSION_SECONDS: i64 = 30 * 24 * 60 * 60;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LoginSession {
    pub token_hash: String,
    pub csrf_hash: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub previous_csrf_hashes: Vec<String>,
    pub expires_at: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WebAuthFile {
    pub username: String,
    pub password_hash: String,
    #[serde(default)]
    pub sessions: Vec<LoginSession>,
}

#[derive(Clone, Debug)]
pub struct NewLogin {
    pub token: String,
    pub csrf: String,
    pub expires_at: i64,
}

fn random_token() -> String {
    let bytes: [u8; 32] = rand::random();
    hex::encode(bytes)
}

fn hash_secret(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}

fn lock_file(paths: &AppPaths) -> Result<std::fs::File> {
    let path = paths.config.join("web-auth.lock");
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)?;
    set_private_file(&path)?;
    file.lock_exclusive()?;
    Ok(file)
}

fn read_auth(path: &Path) -> Result<WebAuthFile> {
    set_private_file(path)?;
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))
}

fn password_hash(password: &str) -> Result<String> {
    let salt_bytes: [u8; 16] = rand::random();
    let salt = SaltString::encode_b64(&salt_bytes).map_err(|e| anyhow::anyhow!(e.to_string()))?;
    Ok(Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!(e.to_string()))?
        .to_string())
}

fn password_matches(encoded: &str, password: &str) -> bool {
    let Ok(hash) = PasswordHash::new(encoded) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &hash)
        .is_ok()
}

fn add_session(auth: &mut WebAuthFile) -> NewLogin {
    let token = random_token();
    let csrf = random_token();
    let expires_at = chrono::Utc::now().timestamp() + SESSION_SECONDS;
    auth.sessions
        .retain(|s| s.expires_at > chrono::Utc::now().timestamp());
    auth.sessions.push(LoginSession {
        token_hash: hash_secret(&token),
        csrf_hash: hash_secret(&csrf),
        previous_csrf_hashes: Vec::new(),
        expires_at,
    });
    NewLogin {
        token,
        csrf,
        expires_at,
    }
}

pub fn has_user(paths: &AppPaths) -> bool {
    paths.web_auth_file().is_file()
}

pub fn signup(paths: &AppPaths, username: &str, password: &str) -> Result<NewLogin> {
    let username = username.trim();
    if username.is_empty() {
        bail!("Username is required");
    }
    if password.len() < 8 {
        bail!("Password must be at least 8 characters");
    }
    let lock = lock_file(paths)?;
    let path = paths.web_auth_file();
    if path.exists() {
        FileExt::unlock(&lock)?;
        bail!("A Web UI user already exists");
    }
    let mut auth = WebAuthFile {
        username: username.to_owned(),
        password_hash: password_hash(password)?,
        sessions: Vec::new(),
    };
    let login = add_session(&mut auth);
    atomic_private_json_create(&path, &auth)?;
    FileExt::unlock(&lock)?;
    Ok(login)
}

pub fn login(paths: &AppPaths, username: &str, password: &str) -> Result<NewLogin> {
    let lock = lock_file(paths)?;
    let path = paths.web_auth_file();
    let mut auth = read_auth(&path).context("Invalid username or password")?;
    let matches = password_matches(&auth.password_hash, password);
    if auth.username != username.trim() || !matches {
        FileExt::unlock(&lock)?;
        bail!("Invalid username or password");
    }
    let login = add_session(&mut auth);
    atomic_private_json(&path, &auth)?;
    FileExt::unlock(&lock)?;
    Ok(login)
}

pub fn validate(paths: &AppPaths, token: &str) -> Result<LoginSession> {
    let auth = read_auth(&paths.web_auth_file())?;
    let token_hash = hash_secret(token);
    let now = chrono::Utc::now().timestamp();
    auth.sessions
        .into_iter()
        .find(|s| s.expires_at > now && constant_eq(&s.token_hash, &token_hash))
        .context("Authentication required")
}

pub fn validate_csrf(session: &LoginSession, csrf: &str) -> bool {
    let hash = hash_secret(csrf);
    let mut valid = constant_eq(&session.csrf_hash, &hash);
    for previous in &session.previous_csrf_hashes {
        valid |= constant_eq(previous, &hash);
    }
    valid
}

/// Called once before the Web server starts listening. Bootstrap then reads only
/// memory. Previously issued hashes keep already-open tabs valid across restarts;
/// all tokens share the login's expiry, logout and reset lifetime.
pub fn prepare_csrf_tokens(paths: &AppPaths) -> Result<std::collections::BTreeMap<String, String>> {
    let lock = lock_file(paths)?;
    let path = paths.web_auth_file();
    let mut tokens = std::collections::BTreeMap::new();
    if path.exists() {
        let mut auth = read_auth(&path)?;
        let now = chrono::Utc::now().timestamp();
        auth.sessions.retain(|session| session.expires_at > now);
        for session in &mut auth.sessions {
            let csrf = random_token();
            session.previous_csrf_hashes.push(std::mem::replace(
                &mut session.csrf_hash,
                hash_secret(&csrf),
            ));
            tokens.insert(session.token_hash.clone(), csrf);
        }
        atomic_private_json(&path, &auth)?;
    }
    FileExt::unlock(&lock)?;
    Ok(tokens)
}

pub fn logout(paths: &AppPaths, token: &str) -> Result<()> {
    let lock = lock_file(paths)?;
    let path = paths.web_auth_file();
    let mut auth = read_auth(&path)?;
    let token_hash = hash_secret(token);
    auth.sessions
        .retain(|s| !constant_eq(&s.token_hash, &token_hash));
    atomic_private_json(&path, &auth)?;
    FileExt::unlock(&lock)?;
    Ok(())
}

pub fn reset(paths: &AppPaths) -> Result<()> {
    let lock = lock_file(paths)?;
    let path = paths.web_auth_file();
    if path.exists() {
        fs::remove_file(path)?;
    }
    FileExt::unlock(&lock)?;
    Ok(())
}

fn constant_eq(left: &str, right: &str) -> bool {
    use subtle::ConstantTimeEq;
    left.as_bytes().ct_eq(right.as_bytes()).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_signup_publishes_exactly_one_complete_identity() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths {
            config: temp.path().join("config"),
            data: temp.path().join("data"),
            runtime: temp.path().join("runtime"),
        };
        paths.ensure().unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let results = std::thread::scope(|scope| {
            (0..8)
                .map(|index| {
                    let paths = &paths;
                    let barrier = barrier.clone();
                    scope.spawn(move || {
                        barrier.wait();
                        (
                            index,
                            signup(paths, &format!("user-{index}"), "fixture-password"),
                        )
                    })
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|worker| worker.join().unwrap())
                .collect::<Vec<_>>()
        });
        let winners = results
            .iter()
            .filter(|(_, result)| result.is_ok())
            .collect::<Vec<_>>();
        assert_eq!(winners.len(), 1);
        let (index, result) = winners[0];
        let auth = read_auth(&paths.web_auth_file()).unwrap();
        assert_eq!(auth.username, format!("user-{index}"));
        assert_eq!(auth.sessions.len(), 1);
        assert!(auth.password_hash.starts_with("$argon2id$"));
        assert!(validate(&paths, &result.as_ref().unwrap().token).is_ok());
    }

    #[test]
    fn signup_login_validate_reset() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths {
            config: temp.path().join("config"),
            data: temp.path().join("data"),
            runtime: temp.path().join("run"),
        };
        paths.ensure().unwrap();
        let first = signup(&paths, "kitten", "password1").unwrap();
        assert!(validate(&paths, &first.token).is_ok());
        assert!(validate_csrf(
            &validate(&paths, &first.token).unwrap(),
            &first.csrf
        ));
        assert!(login(&paths, "kitten", "wrong-password").is_err());
        let second = login(&paths, "kitten", "password1").unwrap();
        logout(&paths, &second.token).unwrap();
        assert!(validate(&paths, &second.token).is_err());
        reset(&paths).unwrap();
        assert!(!has_user(&paths));
    }

    #[test]
    fn restart_keeps_open_tabs_valid_and_persists_only_token_hashes() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths {
            config: temp.path().join("config"),
            data: temp.path().join("data"),
            runtime: temp.path().join("run"),
        };
        paths.ensure().unwrap();
        assert!(prepare_csrf_tokens(&paths).unwrap().is_empty());
        let first = signup(&paths, "kitten", "password1").unwrap();
        let other = login(&paths, "kitten", "password1").unwrap();
        fs::set_permissions(paths.web_auth_file(), fs::Permissions::from_mode(0o644)).unwrap();
        let initial = validate(&paths, &first.token).unwrap();
        assert_eq!(
            fs::metadata(paths.web_auth_file())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let restart = prepare_csrf_tokens(&paths).unwrap();
        let next_restart = prepare_csrf_tokens(&paths).unwrap();
        let current = validate(&paths, &first.token).unwrap();
        assert_eq!(current.expires_at, initial.expires_at);
        for token in [
            &first.csrf,
            &restart[&current.token_hash],
            &next_restart[&current.token_hash],
        ] {
            assert!(validate_csrf(&current, token));
            assert!(!validate_csrf(
                &validate(&paths, &other.token).unwrap(),
                token
            ));
            assert!(
                !fs::read_to_string(paths.web_auth_file())
                    .unwrap()
                    .contains(token)
            );
        }
        assert!(!validate_csrf(&current, "wrong"));
        assert!(
            !fs::read_to_string(paths.web_auth_file())
                .unwrap()
                .contains(&first.token)
        );
        logout(&paths, &first.token).unwrap();
        assert!(validate(&paths, &first.token).is_err());
        assert!(validate(&paths, &other.token).is_ok());
        assert!(
            !prepare_csrf_tokens(&paths)
                .unwrap()
                .contains_key(&current.token_hash)
        );
        reset(&paths).unwrap();
        assert!(prepare_csrf_tokens(&paths).unwrap().is_empty());
    }
}
