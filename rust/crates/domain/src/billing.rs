//! Wire contracts shared by financial admission and the ledger service.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReserveRequest {
    pub id: String,
    pub tenant_id: String,
    pub project_id: String,
    pub api_key_id: String,
    pub request_id: String,
    pub model_id: String,
    pub model_revision: String,
    pub price_version: String,
    pub input_token_limit: i64,
    pub output_token_limit: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SettleRequest {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub latency_ms: i64,
    pub endpoint_id: String,
    pub region: String,
    /// Attests known usage, including partial cancelled usage. Unknown is not zero.
    pub status: String,
}
