use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub mod billing;
pub mod routing;
pub use routing::{PutRoutePolicy, RoutePolicy, RoutePolicySpec, RoutePool};

pub const DEFAULT_MODEL_ID: &str = "xscope-demo";
pub const DEFAULT_PRICE_VERSION: &str = "2026-09-01";
pub const MICROS_PER_MINOR_UNIT: i64 = 1_000_000;

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
pub struct Money {
    pub currency: String,
    pub amount: i64,
}

impl Money {
    #[must_use]
    pub fn cny(amount: i64) -> Self {
        Self {
            currency: "CNY".to_owned(),
            amount,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct Model {
    pub id: String,
    pub display_name: String,
    pub max_context_tokens: i64,
    pub input_per_million_tokens: Money,
    pub output_per_million_tokens: Money,
    pub price_version: String,
}

impl Default for Model {
    fn default() -> Self {
        Self {
            id: DEFAULT_MODEL_ID.to_owned(),
            display_name: "XScope Demo Model".to_owned(),
            max_context_tokens: 32_768,
            input_per_million_tokens: Money::cny(1_000),
            output_per_million_tokens: Money::cny(2_000),
            price_version: DEFAULT_PRICE_VERSION.to_owned(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct Project {
    pub id: String,
    pub tenant_id: String,
    pub name: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ApiKey {
    pub id: String,
    pub tenant_id: String,
    pub project_id: String,
    pub name: String,
    pub scopes: Vec<String>,
    pub allowed_models: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
    pub rate_limit_rpm: i32,
    pub rate_limit_tpm: i64,
    pub monthly_budget: Money,
    pub created_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revoked_at: Option<DateTime<Utc>>,
    pub status: KeyStatus,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyStatus {
    Active,
    Expired,
    Revoked,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ApiKeyRequest {
    pub id: String,
    pub tenant_id: String,
    pub project_id: String,
    pub name: String,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default)]
    pub allowed_models: Vec<String>,
    pub expires_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub rate_limit_rpm: i32,
    #[serde(default)]
    pub rate_limit_tpm: i64,
    #[serde(default)]
    pub monthly_budget: Money,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct IssuedApiKey {
    #[serde(flatten)]
    pub api_key: ApiKey,
    pub secret: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Quote {
    pub model_id: String,
    pub price_version: String,
    pub maximum: Money,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct GatewaySnapshot {
    pub generated_at: DateTime<Utc>,
    pub keys: Vec<GatewayKey>,
    pub route_policies: Vec<RoutePolicy>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct GatewayKey {
    pub id: String,
    pub tenant_id: String,
    pub project_id: String,
    pub secret_hash: String,
    pub scopes: Vec<String>,
    pub allowed_models: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
    pub rate_limit_rpm: i32,
    pub rate_limit_tpm: i64,
    pub monthly_budget: Money,
    pub current_month_spend: Money,
    pub balance_enforced: bool,
    pub available_balance: Money,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct UsageEvent {
    pub schema_version: String,
    pub event_id: String,
    pub request_id: String,
    pub occurred_at: DateTime<Utc>,
    pub tenant_id: String,
    pub project_id: String,
    pub api_key_id: String,
    pub model_id: String,
    pub model_revision: String,
    #[serde(default)]
    pub endpoint_id: String,
    pub region: String,
    pub price_version: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cached_input_tokens: i64,
    pub latency_ms: i64,
    pub status: String,
    #[serde(default)]
    pub cost: Money,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProjectBilling {
    pub project_id: String,
    pub requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cost: Money,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BillingSummary {
    pub period_start: DateTime<Utc>,
    pub period_end: DateTime<Utc>,
    pub requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total: Money,
    pub projects: Vec<ProjectBilling>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BillingAccount {
    pub id: String,
    pub tenant_id: String,
    pub project_id: String,
    pub currency: String,
    pub balance: Money,
    pub enforce_balance: bool,
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CreateOrderRequest {
    pub id: String,
    pub tenant_id: String,
    pub project_id: String,
    pub amount: Money,
    #[serde(default)]
    pub description: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BillingOrder {
    pub id: String,
    pub tenant_id: String,
    pub project_id: String,
    pub kind: String,
    pub amount: Money,
    pub status: String,
    pub description: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CapturePaymentRequest {
    pub id: String,
    pub provider: String,
    pub provider_reference: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Payment {
    pub id: String,
    pub order_id: String,
    pub provider: String,
    pub provider_reference: String,
    pub amount: Money,
    pub status: String,
    pub paid_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CreateRefundRequest {
    pub id: String,
    pub payment_id: String,
    pub amount: Money,
    pub reason: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Refund {
    pub id: String,
    pub payment_id: String,
    pub amount: Money,
    pub reason: String,
    pub status: String,
    pub provider_reference: Option<String>,
    pub created_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LedgerEntry {
    pub id: String,
    pub transaction_id: String,
    pub billing_account_id: String,
    pub ledger_account: String,
    pub amount_microunits: i64,
    pub currency: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PlatformUser {
    pub id: String,
    pub external_subject: String,
    pub username: String,
    pub email: String,
    pub status: String,
    pub last_login_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub memberships: Vec<TenantMembership>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TenantMembership {
    pub tenant_id: String,
    pub role: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct UpdateTenantMembership {
    pub role: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LedgerTransaction {
    pub id: String,
    pub tenant_id: String,
    pub kind: String,
    pub reference_type: String,
    pub reference_id: String,
    pub description: String,
    pub created_at: DateTime<Utc>,
    pub entries: Vec<LedgerEntry>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CreateInvoiceRequest {
    pub id: String,
    pub tenant_id: String,
    pub project_id: String,
    pub period_start: DateTime<Utc>,
    pub period_end: DateTime<Utc>,
    pub title: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Invoice {
    pub id: String,
    pub tenant_id: String,
    pub project_id: String,
    pub period_start: DateTime<Utc>,
    pub period_end: DateTime<Utc>,
    pub amount: Money,
    pub status: String,
    pub title: String,
    pub issued_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReconciliationReport {
    pub generated_at: DateTime<Utc>,
    pub provider: String,
    pub matched: i64,
    pub platform_only: Vec<String>,
    pub provider_only: Vec<String>,
    pub amount_mismatches: BTreeMap<String, Money>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProviderSettlement {
    pub provider_reference: String,
    pub amount: Money,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReconcileRequest {
    pub tenant_id: String,
    pub provider: String,
    #[serde(default)]
    pub settlements: Vec<ProviderSettlement>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct UpdateBalancePolicy {
    pub enforce_balance: bool,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum DomainError {
    #[error("{0} is required")]
    Required(&'static str),
    #[error("unsupported currency")]
    UnsupportedCurrency,
    #[error("amount must be non-negative")]
    NegativeAmount,
    #[error("token counts must be non-negative")]
    NegativeTokens,
    #[error("usage cost exceeds supported range")]
    CostOverflow,
    #[error("rate_limit_rpm must be between 1 and 1000000")]
    InvalidRpm,
    #[error("rate_limit_tpm must be between 1 and 10000000000")]
    InvalidTpm,
    #[error("expires_at must be in the future")]
    InvalidExpiry,
    #[error("unsupported API key scope")]
    InvalidScope,
    #[error("at least one allowed model is required")]
    MissingModel,
}

impl ApiKeyRequest {
    #[must_use]
    pub fn with_defaults(mut self) -> Self {
        if self.scopes.is_empty() {
            self.scopes.push("chat.completions".to_owned());
        }
        if self.allowed_models.is_empty() {
            self.allowed_models.push(DEFAULT_MODEL_ID.to_owned());
        }
        if self.rate_limit_rpm == 0 {
            self.rate_limit_rpm = 60;
        }
        if self.rate_limit_tpm == 0 {
            self.rate_limit_tpm = 60_000;
        }
        if self.monthly_budget.currency.is_empty() {
            "CNY".clone_into(&mut self.monthly_budget.currency);
        }
        self
    }

    /// Validates the key policy before it is persisted.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError`] when a required field or policy boundary is invalid.
    pub fn validate(&self, now: DateTime<Utc>) -> Result<(), DomainError> {
        for (value, name) in [
            (&self.id, "id"),
            (&self.tenant_id, "tenant_id"),
            (&self.project_id, "project_id"),
            (&self.name, "name"),
        ] {
            if value.trim().is_empty() {
                return Err(DomainError::Required(name));
            }
        }
        if !(1..=1_000_000).contains(&self.rate_limit_rpm) {
            return Err(DomainError::InvalidRpm);
        }
        if !(1..=10_000_000_000).contains(&self.rate_limit_tpm) {
            return Err(DomainError::InvalidTpm);
        }
        validate_money(&self.monthly_budget)?;
        if self.expires_at.is_some_and(|expiry| expiry <= now) {
            return Err(DomainError::InvalidExpiry);
        }
        if !self.scopes.iter().any(|scope| scope == "chat.completions")
            || self.scopes.iter().any(|scope| scope != "chat.completions")
        {
            return Err(DomainError::InvalidScope);
        }
        if self.allowed_models.is_empty() {
            return Err(DomainError::MissingModel);
        }
        Ok(())
    }
}

/// Validates the supported currency and sign of an externally supplied amount.
///
/// # Errors
///
/// Returns [`DomainError`] for unsupported currency or negative amounts.
pub fn validate_money(money: &Money) -> Result<(), DomainError> {
    if money.currency != "CNY" {
        return Err(DomainError::UnsupportedCurrency);
    }
    if money.amount < 0 {
        return Err(DomainError::NegativeAmount);
    }
    Ok(())
}

#[must_use]
pub fn key_status(
    revoked_at: Option<DateTime<Utc>>,
    expires_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> KeyStatus {
    if revoked_at.is_some() {
        KeyStatus::Revoked
    } else if expires_at.is_some_and(|expiry| expiry <= now) {
        KeyStatus::Expired
    } else {
        KeyStatus::Active
    }
}

/// Calculates exact inference cost before rounding to a currency minor unit.
///
/// # Errors
///
/// Returns [`DomainError`] for negative token counts or integer overflow.
pub fn usage_cost_microunits(
    model: &Model,
    input_tokens: i64,
    output_tokens: i64,
) -> Result<i64, DomainError> {
    if input_tokens < 0 || output_tokens < 0 {
        return Err(DomainError::NegativeTokens);
    }
    input_tokens
        .checked_mul(model.input_per_million_tokens.amount)
        .and_then(|input| {
            output_tokens
                .checked_mul(model.output_per_million_tokens.amount)
                .and_then(|output| input.checked_add(output))
        })
        .ok_or(DomainError::CostOverflow)
}

#[must_use]
pub fn ceil_minor_units(microunits: i64) -> i64 {
    if microunits == 0 {
        0
    } else {
        (microunits + MICROS_PER_MINOR_UNIT - 1) / MICROS_PER_MINOR_UNIT
    }
}

#[cfg(test)]
mod tests {
    use super::{Model, ceil_minor_units, usage_cost_microunits};

    #[test]
    fn usage_cost_is_precise_before_display_rounding() {
        let cost = usage_cost_microunits(&Model::default(), 7, 13).unwrap();
        assert_eq!(cost, 33_000);
        assert_eq!(ceil_minor_units(cost), 1);
    }
}
