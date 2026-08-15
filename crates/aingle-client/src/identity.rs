use std::{
    fs,
    path::{Path, PathBuf},
};

use base64::{Engine, engine::general_purpose::STANDARD};
use data_encoding::BASE32_NOPAD;
use directories::ProjectDirs;
use ed25519_dalek::{SigningKey, VerifyingKey};
use rand::rngs::OsRng;

use crate::ClientError;

pub struct Identity {
    signing_key: SigningKey,
    path: PathBuf,
}

impl Identity {
    pub fn default_path() -> Result<PathBuf, ClientError> {
        ProjectDirs::from("dev", "aingle", "aingle")
            .map(|dirs| dirs.config_dir().join("identity"))
            .ok_or_else(|| ClientError::Config("cannot determine identity directory".into()))
    }

    pub fn generate(path: impl AsRef<Path>) -> Result<Self, ClientError> {
        let path = path.as_ref().to_path_buf();
        if path.exists() {
            return Err(ClientError::Config(format!(
                "identity already exists at {}",
                path.display()
            )));
        }
        let identity = Self {
            signing_key: SigningKey::generate(&mut OsRng),
            path,
        };
        identity.save()?;
        Ok(identity)
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self, ClientError> {
        let path = path.as_ref().to_path_buf();
        let encoded =
            fs::read_to_string(&path).map_err(|error| ClientError::Config(error.to_string()))?;
        let encoded = if encoded.trim() == "keychain" {
            keyring::Entry::new("aingle", "identity")
                .and_then(|entry| entry.get_password())
                .map_err(|error| {
                    ClientError::Config(format!("cannot read identity from OS keychain: {error}"))
                })?
        } else {
            encoded.trim().to_owned()
        };
        let bytes = STANDARD
            .decode(encoded)
            .map_err(|error| ClientError::Config(error.to_string()))?;
        let secret: [u8; 32] = bytes
            .try_into()
            .map_err(|_| ClientError::Config("identity has invalid length".into()))?;
        Ok(Self {
            signing_key: SigningKey::from_bytes(&secret),
            path,
        })
    }

    pub fn load_default() -> Result<Self, ClientError> {
        Self::load(Self::default_path()?)
    }

    pub fn public_key(&self) -> VerifyingKey {
        self.signing_key.verifying_key()
    }
    pub fn signing_key(&self) -> &SigningKey {
        &self.signing_key
    }
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn agent_id(&self) -> String {
        let digest = blake3::hash(self.public_key().as_bytes());
        format!(
            "agent_{}",
            BASE32_NOPAD.encode(digest.as_bytes())[..27].to_ascii_lowercase()
        )
    }

    fn save(&self) -> Result<(), ClientError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|error| ClientError::Config(error.to_string()))?;
        }
        let secret = STANDARD.encode(self.signing_key.to_bytes());
        let stored = keyring::Entry::new("aingle", "identity")
            .and_then(|entry| entry.set_password(&secret))
            .is_ok();
        fs::write(
            &self.path,
            if stored {
                "keychain".to_owned()
            } else {
                secret
            },
        )
        .map_err(|error| ClientError::Config(error.to_string()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&self.path, fs::Permissions::from_mode(0o600))
                .map_err(|error| ClientError::Config(error.to_string()))?;
        }
        Ok(())
    }
}
