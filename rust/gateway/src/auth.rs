use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::config::ApiKeyConfig;

#[derive(Clone, Debug)]
pub struct Principal {
    pub api_key_id: String,
    pub tenant_id: String,
    pub project_id: String,
}

struct Credential {
    principal: Principal,
    secret_hash: [u8; 32],
}

pub struct KeySet(Vec<Credential>);

impl KeySet {
    #[must_use]
    pub fn new(configs: &[ApiKeyConfig]) -> Self {
        Self(
            configs
                .iter()
                .map(|config| Credential {
                    principal: Principal {
                        api_key_id: config.id.clone(),
                        tenant_id: config.tenant_id.clone(),
                        project_id: config.project_id.clone(),
                    },
                    secret_hash: Sha256::digest(config.secret.as_bytes()).into(),
                })
                .collect(),
        )
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    #[must_use]
    pub fn authenticate(&self, authorization: Option<&str>) -> Option<Principal> {
        let secret = authorization?.strip_prefix("Bearer ")?;
        let candidate: [u8; 32] = Sha256::digest(secret.as_bytes()).into();
        self.0
            .iter()
            .find(|credential| bool::from(credential.secret_hash.ct_eq(&candidate)))
            .map(|credential| credential.principal.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::KeySet;
    use crate::config::ApiKeyConfig;

    #[test]
    fn authenticates_bearer_secret_without_retaining_plaintext() {
        let keys = KeySet::new(&[ApiKeyConfig {
            id: "key-1".into(),
            tenant_id: "tenant-1".into(),
            project_id: "project-1".into(),
            secret: "secret-value".into(),
        }]);
        let principal = keys.authenticate(Some("Bearer secret-value")).unwrap();
        assert_eq!(principal.project_id, "project-1");
        assert!(keys.authenticate(Some("Bearer wrong")).is_none());
    }
}
