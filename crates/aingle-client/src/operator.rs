use std::{fs, path::PathBuf};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::{AgentClaim, ClientError, OperatorDeviceAuthorization};

const OPERATOR_SERVICE: &str = "aingle";
const OPERATOR_ACCOUNT: &str = "operator-session";

#[derive(Debug, Clone)]
pub struct OperatorSession {
    token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingAgentClaim {
    pub claim: AgentClaim,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingOperatorLogin {
    pub authorization: OperatorDeviceAuthorization,
}

impl OperatorSession {
    pub fn new(token: String) -> Result<Self, ClientError> {
        if !token.starts_with("aingle_ops_") {
            return Err(ClientError::Config("invalid operator session token".into()));
        }
        Ok(Self { token })
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn save(&self) -> Result<(), ClientError> {
        let path = operator_session_path()?;
        ensure_parent(&path)?;
        let stored = keyring::Entry::new(OPERATOR_SERVICE, OPERATOR_ACCOUNT)
            .and_then(|entry| entry.set_password(&self.token))
            .is_ok();
        write_private(&path, if stored { "keychain" } else { &self.token })
    }

    pub fn load() -> Result<Self, ClientError> {
        let path = operator_session_path()?;
        let stored = fs::read_to_string(&path)
            .map_err(|error| ClientError::Config(format!("operator is not logged in: {error}")))?;
        let token = if stored.trim() == "keychain" {
            keyring::Entry::new(OPERATOR_SERVICE, OPERATOR_ACCOUNT)
                .and_then(|entry| entry.get_password())
                .map_err(|error| {
                    ClientError::Config(format!(
                        "cannot read operator session from OS keychain: {error}"
                    ))
                })?
        } else {
            stored.trim().to_owned()
        };
        Self::new(token)
    }

    pub fn delete() -> Result<(), ClientError> {
        let path = operator_session_path()?;
        let _ = keyring::Entry::new(OPERATOR_SERVICE, OPERATOR_ACCOUNT)
            .and_then(|entry| entry.delete_credential());
        if path.exists() {
            fs::remove_file(path).map_err(|error| ClientError::Config(error.to_string()))?;
        }
        Ok(())
    }
}

impl PendingAgentClaim {
    pub fn save(&self) -> Result<(), ClientError> {
        save_json(claim_path()?, self)
    }

    pub fn load() -> Result<Self, ClientError> {
        load_json(claim_path()?, "no pending agent claim")
    }

    pub fn delete() -> Result<(), ClientError> {
        delete_if_present(claim_path()?)
    }
}

impl PendingOperatorLogin {
    pub fn save(&self) -> Result<(), ClientError> {
        save_json(operator_login_path()?, self)
    }

    pub fn load() -> Result<Self, ClientError> {
        load_json(operator_login_path()?, "no pending operator login")
    }

    pub fn delete() -> Result<(), ClientError> {
        delete_if_present(operator_login_path()?)
    }
}

fn save_json(path: PathBuf, value: &impl Serialize) -> Result<(), ClientError> {
    let encoded =
        serde_json::to_string(value).map_err(|error| ClientError::Config(error.to_string()))?;
    ensure_parent(&path)?;
    write_private(&path, &encoded)
}

fn load_json<T: DeserializeOwned>(path: PathBuf, missing: &str) -> Result<T, ClientError> {
    let encoded = fs::read_to_string(path)
        .map_err(|error| ClientError::Config(format!("{missing}: {error}")))?;
    serde_json::from_str(&encoded).map_err(|error| ClientError::Config(error.to_string()))
}

fn delete_if_present(path: PathBuf) -> Result<(), ClientError> {
    if path.exists() {
        fs::remove_file(path).map_err(|error| ClientError::Config(error.to_string()))?;
    }
    Ok(())
}

fn operator_session_path() -> Result<PathBuf, ClientError> {
    config_dir().map(|path| path.join("operator-session"))
}

fn operator_login_path() -> Result<PathBuf, ClientError> {
    config_dir().map(|path| path.join("operator-login.json"))
}

fn claim_path() -> Result<PathBuf, ClientError> {
    config_dir().map(|path| path.join("agent-claim.json"))
}

fn config_dir() -> Result<PathBuf, ClientError> {
    ProjectDirs::from("dev", "aingle", "aingle")
        .map(|dirs| dirs.config_dir().to_path_buf())
        .ok_or_else(|| ClientError::Config("cannot determine configuration directory".into()))
}

fn ensure_parent(path: &PathBuf) -> Result<(), ClientError> {
    fs::create_dir_all(path.parent().expect("private state path has parent"))
        .map_err(|error| ClientError::Config(error.to_string()))
}

fn write_private(path: &PathBuf, value: &str) -> Result<(), ClientError> {
    fs::write(path, value).map_err(|error| ClientError::Config(error.to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|error| ClientError::Config(error.to_string()))?;
    }
    Ok(())
}
