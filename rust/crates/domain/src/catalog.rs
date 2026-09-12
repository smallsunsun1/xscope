//! Versioned public model definitions. Transport addresses are internal-only.
use serde::{Deserialize, Serialize};

use crate::Model;

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogModel {
    pub revision: i64,
    pub enabled: bool,
    pub default_pool: Option<String>,
    pub model: Model,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PutModel {
    pub expected_revision: i64,
    pub enabled: bool,
    pub default_pool: Option<String>,
    pub model: Model,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServingEndpoint {
    pub id: String,
    pub model: String,
    pub revision: String,
    /// Envoy/llm-d entry Service, never a model Pod address.
    pub address: String,
    #[serde(default)]
    pub tls: bool,
    #[serde(default)]
    pub server_name: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogSnapshot {
    pub models: Vec<CatalogModel>,
    pub endpoints: Vec<ServingEndpoint>,
}

pub fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

impl Model {
    pub fn validate(&self) -> Result<(), String> {
        if !identifier(&self.id)
            || !identifier(&self.price_version)
            || self.display_name.trim().is_empty()
            || self.display_name.len() > 256
            || !(1..=16_777_216).contains(&self.max_context_tokens)
        {
            return Err("invalid model identity, display name or context limit".into());
        }
        for price in [
            &self.input_per_million_tokens,
            &self.output_per_million_tokens,
        ] {
            if price.currency != "CNY"
                || price.amount < 0
                || price.amount.checked_mul(self.max_context_tokens).is_none()
            {
                return Err(
                    "model prices require nonnegative CNY amounts within integer bounds".into(),
                );
            }
        }
        self.max_context_tokens
            .checked_mul(
                self.input_per_million_tokens
                    .amount
                    .max(self.output_per_million_tokens.amount),
            )
            .ok_or("model price exceeds integer bounds")?;
        Ok(())
    }
}

impl ServingEndpoint {
    pub fn validate(&self) -> Result<(), String> {
        if !identifier(&self.id) || !identifier(&self.model) || !identifier(&self.revision) {
            return Err("invalid serving endpoint identity".into());
        }
        let (host, port) = self
            .address
            .rsplit_once(':')
            .ok_or("entry address requires host:port")?;
        if host.is_empty()
            || host.len() > 253
            || !host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".-[]:".contains(&b))
            || !port.parse::<u16>().is_ok_and(|p| p > 0)
            || self.server_name.len() > 253
            || !self
                .server_name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".-".contains(&b))
            || (self.tls && self.server_name.is_empty())
        {
            return Err("invalid serving Service address or TLS name".into());
        }
        Ok(())
    }
}

impl CatalogSnapshot {
    pub fn validate(&self) -> Result<(), String> {
        let mut models = std::collections::HashSet::new();
        for entry in &self.models {
            entry.model.validate()?;
            if entry.revision <= 0
                || !models.insert(&entry.model.id)
                || entry
                    .default_pool
                    .as_ref()
                    .is_some_and(|id| !identifier(id))
            {
                return Err("invalid or duplicate catalog model".into());
            }
        }
        let mut endpoints = std::collections::HashSet::new();
        for endpoint in &self.endpoints {
            endpoint.validate()?;
            if !models.contains(&endpoint.model) || !endpoints.insert(&endpoint.id) {
                return Err("duplicate endpoint or unregistered model".into());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reject_invalid_prices_and_duplicate_models() {
        let mut model = Model::default();
        assert!(model.validate().is_ok());
        model.output_per_million_tokens.amount = i64::MAX;
        assert!(model.validate().is_err());
        let entry = CatalogModel {
            revision: 1,
            enabled: true,
            default_pool: None,
            model: Model::default(),
        };
        assert!(
            CatalogSnapshot {
                models: vec![entry.clone(), entry],
                endpoints: vec![]
            }
            .validate()
            .is_err()
        );
    }
}
