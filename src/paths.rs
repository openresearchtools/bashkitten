use anyhow::{Context, Result};
use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct AppPaths {
    pub config: PathBuf,
    pub data: PathBuf,
    pub runtime: PathBuf,
}

impl AppPaths {
    pub fn discover() -> Result<Self> {
        let home = env::var_os("HOME")
            .map(PathBuf::from)
            .context("HOME is not set")?;
        let config = env::var_os("BASHKITTEN_CONFIG_DIR")
            .map(PathBuf::from)
            .or_else(|| env::var_os("XDG_CONFIG_HOME").map(|v| PathBuf::from(v).join("bashkitten")))
            .unwrap_or_else(|| home.join(".config/bashkitten"));
        let data = env::var_os("BASHKITTEN_DATA_DIR")
            .map(PathBuf::from)
            .or_else(|| env::var_os("XDG_DATA_HOME").map(|v| PathBuf::from(v).join("bashkitten")))
            .unwrap_or_else(|| home.join(".local/share/bashkitten"));
        let runtime = env::var_os("BASHKITTEN_RUNTIME_DIR")
            .map(PathBuf::from)
            .or_else(|| env::var_os("XDG_RUNTIME_DIR").map(|v| PathBuf::from(v).join("bashkitten")))
            .unwrap_or_else(|| data.join("runtime"));
        Ok(Self {
            config,
            data,
            runtime,
        })
    }

    pub fn ensure(&self) -> Result<()> {
        for dir in [
            &self.config,
            &self.data,
            &self.runtime,
            &self.sessions_dir(),
            &self.skills_dir(),
        ] {
            ensure_private_dir(dir)?;
        }
        Ok(())
    }

    pub fn config_file(&self) -> PathBuf {
        self.config.join("config.json")
    }
    pub fn web_auth_file(&self) -> PathBuf {
        self.config.join("web-auth.json")
    }
    pub fn provider_auth_file(&self) -> PathBuf {
        self.config.join("provider-auth.json")
    }
    pub fn sessions_dir(&self) -> PathBuf {
        self.data.join("sessions")
    }
    pub fn skills_dir(&self) -> PathBuf {
        self.config.join("skills")
    }
    pub fn session_dir(&self, id: &str) -> PathBuf {
        self.sessions_dir().join(id)
    }
}

pub fn ensure_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| format!("create {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("chmod 0700 {}", path.display()))?;
    Ok(())
}

pub fn set_private_file(path: &Path) -> Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("chmod 0600 {}", path.display()))
}
