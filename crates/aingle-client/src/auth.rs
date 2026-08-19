use std::time::Duration;

use base64::{Engine, engine::general_purpose::STANDARD};
use ed25519_dalek::Signer;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use url::Url;

use crate::Identity;

#[derive(Debug, Clone, Deserialize)]
pub struct Session {
    pub token: String,
    pub expires_at: String,
    pub agent_id: String,
}

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("invalid API URL")]
    InvalidUrl,
    #[error("invalid challenge: {0}")]
    InvalidChallenge(String),
}

pub struct AuthClient {
    base_url: Url,
    http: Client,
}

#[derive(Serialize)]
struct RegisterRequest {
    public_key: String,
    display_name: Option<String>,
}

#[derive(Serialize)]
struct ChallengeRequest {
    agent_id: String,
}

#[derive(Deserialize)]
struct ChallengeResponse {
    challenge_id: String,
    nonce: String,
}

#[derive(Serialize)]
struct SessionRequest {
    agent_id: String,
    challenge_id: String,
    signature: String,
}

impl AuthClient {
    pub fn new(base_url: Url) -> Self {
        Self {
            base_url,
            http: Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(15))
                .build()
                .expect("static Aingle HTTP client configuration is valid"),
        }
    }

    pub async fn register(
        &self,
        identity: &Identity,
        display_name: Option<String>,
    ) -> Result<(), AuthError> {
        let url = self
            .base_url
            .join("/v1/agents")
            .map_err(|_| AuthError::InvalidUrl)?;
        self.http
            .post(url)
            .json(&RegisterRequest {
                public_key: STANDARD.encode(identity.public_key().as_bytes()),
                display_name,
            })
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    pub async fn authenticate(&self, identity: &Identity) -> Result<Session, AuthError> {
        let agent_id = identity.agent_id();
        let challenge_url = self
            .base_url
            .join("/v1/auth/challenge")
            .map_err(|_| AuthError::InvalidUrl)?;
        let challenge: ChallengeResponse = self
            .http
            .post(challenge_url)
            .json(&ChallengeRequest {
                agent_id: agent_id.clone(),
            })
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let nonce = STANDARD
            .decode(&challenge.nonce)
            .map_err(|error| AuthError::InvalidChallenge(error.to_string()))?;
        let signature = identity.signing_key().sign(&nonce);
        let session_url = self
            .base_url
            .join("/v1/auth/session")
            .map_err(|_| AuthError::InvalidUrl)?;
        Ok(self
            .http
            .post(session_url)
            .json(&SessionRequest {
                agent_id,
                challenge_id: challenge.challenge_id,
                signature: STANDARD.encode(signature.to_bytes()),
            })
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }
}
