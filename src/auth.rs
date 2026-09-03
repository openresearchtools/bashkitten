use crate::config::atomic_private_json;
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
    atomic_private_json(&path, &auth)?;
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
    constant_eq(&session.csrf_hash, &hash_secret(csrf))
}

/// Rotate the CSRF token for an authenticated login. This lets a freshly loaded
/// page obtain an in-memory token while the server continues to persist only a
/// hash of that token.
pub fn rotate_csrf(paths: &AppPaths, token: &str) -> Result<String> {
    let lock = lock_file(paths)?;
    let path = paths.web_auth_file();
    let mut auth = read_auth(&path)?;
    let token_hash = hash_secret(token);
    let now = chrono::Utc::now().timestamp();
    let session = auth
        .sessions
        .iter_mut()
        .find(|s| s.expires_at > now && constant_eq(&s.token_hash, &token_hash))
        .context("Authentication required")?;
    let csrf = random_token();
    session.csrf_hash = hash_secret(&csrf);
    atomic_private_json(&path, &auth)?;
    FileExt::unlock(&lock)?;
    Ok(csrf)
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
}
