use std::time::Duration;

use base64::{Engine, engine::general_purpose::STANDARD};
use ed25519_dalek::Signer;
use rand::{RngCore, rngs::OsRng};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use url::Url;

use crate::Identity;

#[derive(Debug, Clone, Deserialize)]
pub struct Session {
    pub token: String,
    pub expires_at: String,
    pub agent_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentClaim {
    pub status: String,
    pub agent_id: String,
    pub claim_token: String,
    pub verification_uri: String,
    pub user_code: String,
    pub expires_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentClaimStatus {
    pub agent_id: String,
    pub status: String,
    pub expires_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperatorDeviceAuthorization {
    pub status: String,
    pub device_code: String,
    pub user_code: String,
    pub session_token: String,
    pub verification_uri: String,
    pub expires_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperatorDeviceStatus {
    pub status: String,
    pub operator_id: Option<String>,
    pub expires_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperatorProfile {
    pub id: String,
    pub email: String,
    pub status: String,
    pub identities: Vec<OperatorIdentity>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperatorIdentity {
    pub provider: String,
    pub email: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrollmentCapability {
    pub token: String,
    pub max_uses: u32,
    pub expires_at: String,
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
struct AgentActivationRequest {
    public_key: String,
    nonce: String,
    signature: String,
    display_name: Option<String>,
}

#[derive(Serialize)]
struct EnrollRequest {
    enrollment_token: String,
    public_key: String,
    nonce: String,
    signature: String,
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

#[derive(Serialize)]
struct ClaimStatusRequest<'a> {
    claim_token: &'a str,
}

#[derive(Serialize)]
struct DeviceStatusRequest<'a> {
    device_code: &'a str,
}

#[derive(Serialize)]
struct EnrollmentCapabilityRequest {
    max_uses: u32,
    expires_in_seconds: u64,
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

    pub async fn create_claim(
        &self,
        identity: &Identity,
        display_name: Option<String>,
    ) -> Result<AgentClaim, AuthError> {
        let nonce = random_nonce();
        let signature = identity.signing_key().sign(&activation_statement(
            ActivationKind::Claim,
            &nonce,
            display_name.as_deref(),
            None,
        ));
        let url = self
            .base_url
            .join("/v1/agent-claims")
            .map_err(|_| AuthError::InvalidUrl)?;
        Ok(self
            .http
            .post(url)
            .json(&AgentActivationRequest {
                public_key: STANDARD.encode(identity.public_key().as_bytes()),
                nonce: STANDARD.encode(nonce),
                signature: STANDARD.encode(signature.to_bytes()),
                display_name,
            })
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    pub async fn claim_status(&self, claim_token: &str) -> Result<AgentClaimStatus, AuthError> {
        let url = self
            .base_url
            .join("/v1/agent-claims/status")
            .map_err(|_| AuthError::InvalidUrl)?;
        Ok(self
            .http
            .post(url)
            .json(&ClaimStatusRequest { claim_token })
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    pub async fn enroll(
        &self,
        enrollment_token: String,
        identity: &Identity,
        display_name: Option<String>,
    ) -> Result<serde_json::Value, AuthError> {
        let nonce = random_nonce();
        let statement = activation_statement(
            ActivationKind::Enroll,
            &nonce,
            display_name.as_deref(),
            Some(&enrollment_token),
        );
        let signature = identity.signing_key().sign(&statement);
        let url = self
            .base_url
            .join("/v1/agents/enroll")
            .map_err(|_| AuthError::InvalidUrl)?;
        Ok(self
            .http
            .post(url)
            .json(&EnrollRequest {
                enrollment_token,
                public_key: STANDARD.encode(identity.public_key().as_bytes()),
                nonce: STANDARD.encode(nonce),
                signature: STANDARD.encode(signature.to_bytes()),
                display_name,
            })
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    pub async fn create_operator_device_authorization(
        &self,
    ) -> Result<OperatorDeviceAuthorization, AuthError> {
        let url = self
            .base_url
            .join("/v1/operator/device-authorizations")
            .map_err(|_| AuthError::InvalidUrl)?;
        Ok(self
            .http
            .post(url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    pub async fn operator_device_status(
        &self,
        device_code: &str,
    ) -> Result<OperatorDeviceStatus, AuthError> {
        let url = self
            .base_url
            .join("/v1/operator/device-authorizations/status")
            .map_err(|_| AuthError::InvalidUrl)?;
        Ok(self
            .http
            .post(url)
            .json(&DeviceStatusRequest { device_code })
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    pub async fn operator_profile(
        &self,
        session_token: &str,
    ) -> Result<OperatorProfile, AuthError> {
        let url = self
            .base_url
            .join("/v1/operator/me")
            .map_err(|_| AuthError::InvalidUrl)?;
        Ok(self
            .http
            .get(url)
            .bearer_auth(session_token)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    pub async fn create_enrollment_capability(
        &self,
        session_token: &str,
        max_uses: u32,
        expires_in_seconds: u64,
    ) -> Result<EnrollmentCapability, AuthError> {
        let url = self
            .base_url
            .join("/v1/operator/enrollment-tokens")
            .map_err(|_| AuthError::InvalidUrl)?;
        Ok(self
            .http
            .post(url)
            .bearer_auth(session_token)
            .json(&EnrollmentCapabilityRequest {
                max_uses,
                expires_in_seconds,
            })
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    pub async fn operator_logout(&self, session_token: &str) -> Result<(), AuthError> {
        let url = self
            .base_url
            .join("/v1/operator/logout")
            .map_err(|_| AuthError::InvalidUrl)?;
        self.http
            .post(url)
            .bearer_auth(session_token)
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

enum ActivationKind {
    Claim,
    Enroll,
}

fn random_nonce() -> [u8; 32] {
    let mut nonce = [0_u8; 32];
    OsRng.fill_bytes(&mut nonce);
    nonce
}

fn activation_statement(
    kind: ActivationKind,
    nonce: &[u8; 32],
    display_name: Option<&str>,
    enrollment_token: Option<&str>,
) -> Vec<u8> {
    let domain = match kind {
        ActivationKind::Claim => b"aingle-agent-claim-v1\0".as_slice(),
        ActivationKind::Enroll => b"aingle-agent-enrollment-v1\0".as_slice(),
    };
    let display_digest = Sha256::digest(display_name.unwrap_or_default().as_bytes());
    let token_digest = enrollment_token
        .map(|token| Sha256::digest(token.as_bytes()).to_vec())
        .unwrap_or_else(|| vec![0_u8; 32]);
    [
        domain,
        nonce.as_slice(),
        display_digest.as_slice(),
        token_digest.as_slice(),
    ]
    .concat()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activation_statements_bind_kind_name_and_enrollment_token() {
        let nonce = [7_u8; 32];
        let claim = activation_statement(ActivationKind::Claim, &nonce, Some("agent"), None);
        let enrollment = activation_statement(
            ActivationKind::Enroll,
            &nonce,
            Some("agent"),
            Some("aingle_enroll_secret"),
        );

        assert_ne!(claim, enrollment);
        assert_ne!(
            enrollment,
            activation_statement(
                ActivationKind::Enroll,
                &nonce,
                Some("other"),
                Some("aingle_enroll_secret")
            )
        );
    }
}
