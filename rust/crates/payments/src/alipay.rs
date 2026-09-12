use crate::{Error, Result, format_amount, identifier, parse_amount};
use aws_lc_rs::{
    rand::SystemRandom,
    signature::{self, RsaKeyPair, RsaSubjectPublicKey},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::{Value, value::RawValue};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, io::Read, path::Path, time::Duration};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub app_id: String,
    pub seller_id: String,
    pub environment: String,
    pub notify_url: String,
    pub private_key_file: String,
    pub alipay_public_key_file: String,
}
fn read(path: &Path, limit: usize) -> Result<Vec<u8>> {
    if !path.is_absolute() {
        return Err(Error::Invalid("private payment paths must be absolute"));
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .map_err(|_| Error::Invalid("private payment file unavailable"))?
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Error::Invalid("private payment read failed"))?;
    if bytes.len() > limit {
        return Err(Error::Invalid("private payment file exceeds bound"));
    }
    Ok(bytes)
}
fn key(path: &str, tag: &str) -> Result<Vec<u8>> {
    let bytes = read(Path::new(path), 16384)?;
    if bytes.starts_with(b"-----BEGIN") {
        let block = pem::parse(&bytes).map_err(|_| Error::Invalid("invalid PEM"))?;
        if block.tag() != tag {
            return Err(Error::Invalid("unsupported payment key encoding"));
        }
        Ok(block.into_contents())
    } else {
        Ok(bytes)
    }
}
pub struct Alipay {
    config: Config,
    signing: RsaKeyPair,
    verifying: RsaSubjectPublicKey,
    http: reqwest::Client,
}
/// No Deserialize/public constructor: only verified provider material creates it.
pub struct VerifiedTrade {
    order: String,
    trade: String,
    amount: i64,
    state: String,
    proof: String,
    evidence: Value,
}
pub struct VerifiedRefund {
    order: String,
    trade: String,
    request: String,
    amount: i64,
    evidence: Value,
}
impl VerifiedRefund {
    pub fn order_id(&self) -> &str {
        &self.order
    }
    pub fn trade_id(&self) -> &str {
        &self.trade
    }
    pub fn request_id(&self) -> &str {
        &self.request
    }
    pub fn amount_minor(&self) -> i64 {
        self.amount
    }
    pub fn evidence(&self) -> &Value {
        &self.evidence
    }
}
impl VerifiedTrade {
    pub fn order_id(&self) -> &str {
        &self.order
    }
    pub fn trade_id(&self) -> &str {
        &self.trade
    }
    pub fn amount_minor(&self) -> i64 {
        self.amount
    }
    pub fn state(&self) -> &str {
        &self.state
    }
    pub fn paid(&self) -> bool {
        matches!(self.state.as_str(), "TRADE_SUCCESS" | "TRADE_FINISHED")
    }
    pub fn proof_sha256(&self) -> &str {
        &self.proof
    }
    pub fn evidence(&self) -> &Value {
        &self.evidence
    }
}
impl Alipay {
    pub fn from_env() -> Result<Option<Self>> {
        let Some(path) = std::env::var_os("XSCOPE_ALIPAY_CONFIG_FILE") else {
            return Ok(None);
        };
        let config: Config = serde_json::from_slice(&read(Path::new(&path), 8192)?)
            .map_err(|_| Error::Invalid("invalid private payment config"))?;
        Self::new(config).map(Some)
    }
    pub fn new(config: Config) -> Result<Self> {
        if !["production", "sandbox"].contains(&config.environment.as_str())
            || !identifier(&config.app_id)
            || !identifier(&config.seller_id)
        {
            return Err(Error::Invalid(
                "invalid payment merchant identity or environment",
            ));
        }
        let notify = reqwest::Url::parse(&config.notify_url)
            .map_err(|_| Error::Invalid("invalid notification URL"))?;
        if notify.scheme() != "https"
            || !notify.username().is_empty()
            || notify.password().is_some()
            || notify.query().is_some()
            || notify.fragment().is_some()
        {
            return Err(Error::Invalid(
                "payment callbacks require HTTPS without URL credentials",
            ));
        }
        let signing = RsaKeyPair::from_pkcs8(&key(&config.private_key_file, "PRIVATE KEY")?)
            .map_err(|_| Error::Invalid("invalid PKCS8 signing key"))?;
        let verifying =
            RsaSubjectPublicKey::from_der(&key(&config.alipay_public_key_file, "PUBLIC KEY")?)
                .map_err(|_| Error::Invalid("invalid Alipay public key"))?;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| Error::Unavailable)?;
        Ok(Self {
            config,
            signing,
            verifying,
            http,
        })
    }
    pub fn profile(&self) -> String {
        format!(
            "{:x}",
            Sha256::digest(format!(
                "{}:{}:{}",
                self.config.environment, self.config.app_id, self.config.seller_id
            ))
        )
    }
    pub fn environment(&self) -> &str {
        &self.config.environment
    }
    fn sign(&self, content: &[u8]) -> Result<String> {
        let mut result = vec![0; self.signing.public_modulus_len()];
        self.signing
            .sign(
                &signature::RSA_PKCS1_SHA256,
                &SystemRandom::new(),
                content,
                &mut result,
            )
            .map_err(|_| Error::Signature)?;
        Ok(STANDARD.encode(result))
    }
    fn verify(&self, content: &[u8], signature: &str) -> Result<()> {
        let bytes = STANDARD.decode(signature).map_err(|_| Error::Signature)?;
        signature::UnparsedPublicKey::new(
            &signature::RSA_PKCS1_2048_8192_SHA256,
            self.verifying.as_ref(),
        )
        .verify(content, &bytes)
        .map_err(|_| Error::Signature)
    }
    pub fn verify_notification(&self, bytes: &[u8]) -> Result<VerifiedTrade> {
        if bytes.len() > 64 * 1024 {
            return Err(Error::Invalid("payment callback exceeds bound"));
        }
        let fields: Vec<(String, String)> = serde_urlencoded::from_bytes(bytes)
            .map_err(|_| Error::Invalid("invalid notification form"))?;
        let mut values = BTreeMap::new();
        for (key, value) in fields {
            if key.is_empty()
                || key.len() > 128
                || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                || value.contains('\u{fffd}')
                || values.insert(key, value).is_some()
            {
                return Err(Error::Invalid("duplicate or invalid notification field"));
            }
        }
        let get = |name: &str| {
            values
                .get(name)
                .map(String::as_str)
                .ok_or(Error::Invalid("notification field missing"))
        };
        if get("sign_type")? != "RSA2"
            || !get("charset")?.eq_ignore_ascii_case("utf-8")
            || get("app_id")? != self.config.app_id
            || get("seller_id")? != self.config.seller_id
        {
            return Err(Error::Invalid(
                "notification merchant, charset or algorithm mismatch",
            ));
        }
        let content = canonical(&values, true);
        self.verify(content.as_bytes(), get("sign")?)?;
        let mut trade = verified_trade(
            get("out_trade_no")?,
            get("trade_no")?,
            parse_amount(get("total_amount")?)?,
            get("trade_status")?,
            content.as_bytes(),
        )?;
        trade.evidence = serde_json::json!({"protocol":"alipay.notify.rsa2","signed_content":content,"signature":get("sign")?});
        Ok(trade)
    }
    async fn request(&self, method: &str, business: Value) -> Result<SignedResponse> {
        let timestamp = (chrono::Utc::now() + chrono::Duration::hours(8))
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        let mut values: BTreeMap<String, String> = [
            ("app_id", self.config.app_id.clone()),
            ("method", method.into()),
            ("format", "JSON".into()),
            ("charset", "utf-8".into()),
            ("sign_type", "RSA2".into()),
            ("timestamp", timestamp),
            ("version", "1.0".into()),
            ("notify_url", self.config.notify_url.clone()),
            ("biz_content", business.to_string()),
        ]
        .into_iter()
        .map(|(k, v)| (k.into(), v))
        .collect();
        values.insert(
            "sign".into(),
            self.sign(canonical(&values, false).as_bytes())?,
        );
        let endpoint = if self.config.environment == "production" {
            "https://openapi.alipay.com/gateway.do"
        } else {
            "https://openapi-sandbox.dl.alipaydev.com/gateway.do"
        };
        let mut response = self
            .http
            .post(endpoint)
            .form(&values)
            .send()
            .await
            .map_err(|_| Error::Unavailable)?;
        if !response.status().is_success() {
            return Err(Error::Unavailable);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| Error::Unavailable)? {
            if bytes.len() + chunk.len() > 256 * 1024 {
                return Err(Error::Invalid("payment response exceeds bound"));
            }
            bytes.extend_from_slice(&chunk);
        }
        self.response(method, &bytes)
    }
    fn response(&self, method: &str, bytes: &[u8]) -> Result<SignedResponse> {
        let packet: Packet = serde_json::from_slice(bytes)
            .map_err(|_| Error::Invalid("invalid signed provider response"))?;
        let raw = match method {
            "alipay.trade.precreate" => packet.alipay_trade_precreate_response,
            "alipay.trade.query" => packet.alipay_trade_query_response,
            "alipay.trade.refund" => packet.alipay_trade_refund_response,
            "alipay.trade.fastpay.refund.query" => {
                packet.alipay_trade_fastpay_refund_query_response
            }
            _ => None,
        }
        .ok_or(Error::Rejected)?;
        self.verify(raw.get().as_bytes(), &packet.sign)?;
        let status: ResponseCode = serde_json::from_str(raw.get())
            .map_err(|_| Error::Invalid("invalid payment response code"))?;
        if status.code != "10000" {
            return Err(Error::Rejected);
        }
        Ok(SignedResponse {
            raw,
            signature: packet.sign,
        })
    }
    pub async fn precreate(&self, order: &str, amount: i64) -> Result<String> {
        if !identifier(order) {
            return Err(Error::Invalid("invalid provider order ID"));
        }
        let raw = self.request("alipay.trade.precreate", serde_json::json!({"out_trade_no":order,"total_amount":format_amount(amount)?,"subject":"XScope platform credit","timeout_express":"30m"})).await?;
        #[derive(Deserialize)]
        struct Reply {
            out_trade_no: String,
            qr_code: String,
        }
        let reply: Reply = serde_json::from_str(raw.get())
            .map_err(|_| Error::Invalid("invalid checkout response"))?;
        if reply.out_trade_no != order
            || !reply.qr_code.starts_with("https://qr.alipay.com/")
            || reply.qr_code.len() > 2048
        {
            return Err(Error::Invalid("checkout binding mismatch"));
        }
        Ok(reply.qr_code)
    }
    pub async fn query(&self, order: &str) -> Result<VerifiedTrade> {
        if !identifier(order) {
            return Err(Error::Invalid("invalid provider order ID"));
        }
        let raw = self
            .request(
                "alipay.trade.query",
                serde_json::json!({"out_trade_no":order}),
            )
            .await?;
        #[derive(Deserialize)]
        struct Reply {
            out_trade_no: String,
            trade_no: String,
            total_amount: Box<RawValue>,
            trade_status: String,
        }
        let reply: Reply = serde_json::from_str(raw.get())
            .map_err(|_| Error::Invalid("invalid trade response"))?;
        if reply.out_trade_no != order {
            return Err(Error::Invalid("trade query binding mismatch"));
        }
        let amount = decimal(&reply.total_amount)?;
        let mut trade = verified_trade(
            &reply.out_trade_no,
            &reply.trade_no,
            amount,
            &reply.trade_status,
            raw.get().as_bytes(),
        )?;
        trade.evidence = serde_json::json!({"protocol":"alipay.trade.query.rsa2","signed_content":raw.get(),"signature":raw.signature});
        Ok(trade)
    }
    /// The same durable out_request_no is used for every retry. A successful
    /// submission is not final financial evidence; query after provider delay.
    pub async fn submit_refund(&self, order: &str, request: &str, amount: i64) -> Result<()> {
        if !identifier(order) || !identifier(request) {
            return Err(Error::Invalid("invalid refund identity"));
        }
        self.request("alipay.trade.refund", serde_json::json!({"out_trade_no":order,"out_request_no":request,"refund_amount":format_amount(amount)?})).await?;
        Ok(())
    }
    pub async fn query_refund(
        &self,
        order: &str,
        trade: &str,
        request: &str,
        amount: i64,
    ) -> Result<VerifiedRefund> {
        if !identifier(order) || !identifier(trade) || !identifier(request) {
            return Err(Error::Invalid("invalid refund identity"));
        }
        let raw = self
            .request(
                "alipay.trade.fastpay.refund.query",
                serde_json::json!({"out_trade_no":order,"out_request_no":request}),
            )
            .await?;
        verified_refund(raw, order, trade, request, amount)
    }
    /// Offline recovery still requires the original RSA2 envelope and exact
    /// order/trade/refund/amount bindings. No deserialized status is trusted.
    pub fn verify_refund_response(
        &self,
        bytes: &[u8],
        order: &str,
        trade: &str,
        request: &str,
        amount: i64,
    ) -> Result<VerifiedRefund> {
        if bytes.len() > 256 * 1024 {
            return Err(Error::Invalid("refund evidence exceeds bound"));
        }
        let raw = self.response("alipay.trade.fastpay.refund.query", bytes)?;
        verified_refund(raw, order, trade, request, amount)
    }
}
fn verified_refund(
    raw: SignedResponse,
    order: &str,
    trade: &str,
    request: &str,
    amount: i64,
) -> Result<VerifiedRefund> {
    #[derive(Deserialize)]
    struct Reply {
        out_trade_no: String,
        trade_no: String,
        out_request_no: String,
        refund_amount: Box<RawValue>,
        refund_status: Option<String>,
    }
    let reply: Reply =
        serde_json::from_str(raw.get()).map_err(|_| Error::Invalid("invalid refund query"))?;
    if reply.out_trade_no != order
        || reply.trade_no != trade
        || reply.out_request_no != request
        || decimal(&reply.refund_amount)? != amount
    {
        return Err(Error::Invalid("refund evidence binding mismatch"));
    }
    if reply.refund_status.as_deref() != Some("REFUND_SUCCESS") {
        return Err(Error::Unavailable);
    }
    Ok(VerifiedRefund {
        order: order.into(),
        trade: trade.into(),
        request: request.into(),
        amount,
        evidence: serde_json::json!({"protocol":"alipay.refund.query.rsa2","signed_content":raw.get(),"signature":raw.signature}),
    })
}
fn canonical(values: &BTreeMap<String, String>, notification: bool) -> String {
    values
        .iter()
        .filter(|(key, value)| {
            key.as_str() != "sign"
                && !(notification && key.as_str() == "sign_type")
                && !value.is_empty()
        })
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("&")
}
fn verified_trade(
    order: &str,
    trade: &str,
    amount: i64,
    state: &str,
    proof: &[u8],
) -> Result<VerifiedTrade> {
    if !identifier(order)
        || !identifier(trade)
        || ![
            "WAIT_BUYER_PAY",
            "TRADE_SUCCESS",
            "TRADE_FINISHED",
            "TRADE_CLOSED",
        ]
        .contains(&state)
    {
        return Err(Error::Invalid("invalid trade identity or state"));
    }
    Ok(VerifiedTrade {
        order: order.into(),
        trade: trade.into(),
        amount,
        state: state.into(),
        proof: format!("{:x}", Sha256::digest(proof)),
        evidence: Value::Null,
    })
}
fn decimal(raw: &RawValue) -> Result<i64> {
    if raw.get().starts_with('"') {
        parse_amount(
            &serde_json::from_str::<String>(raw.get())
                .map_err(|_| Error::Invalid("invalid amount string"))?,
        )
    } else {
        parse_amount(raw.get())
    }
}
#[derive(Deserialize)]
struct ResponseCode {
    code: String,
}
struct SignedResponse {
    raw: Box<RawValue>,
    signature: String,
}
impl SignedResponse {
    fn get(&self) -> &str {
        self.raw.get()
    }
}
#[derive(Deserialize)]
struct Packet {
    sign: String,
    alipay_trade_precreate_response: Option<Box<RawValue>>,
    alipay_trade_query_response: Option<Box<RawValue>>,
    alipay_trade_refund_response: Option<Box<RawValue>>,
    alipay_trade_fastpay_refund_query_response: Option<Box<RawValue>>,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use aws_lc_rs::{rsa::KeySize, signature::KeyPair};
    fn client() -> (Alipay, RsaKeyPair) {
        let signing = RsaKeyPair::generate(KeySize::Rsa2048).unwrap();
        let provider = RsaKeyPair::generate(KeySize::Rsa2048).unwrap();
        let verifying = RsaSubjectPublicKey::from_der(provider.public_key().as_ref()).unwrap();
        (
            Alipay {
                config: Config {
                    app_id: "synthetic-app".into(),
                    seller_id: "synthetic-seller".into(),
                    environment: "sandbox".into(),
                    notify_url: "https://example.invalid/notify".into(),
                    private_key_file: "/synthetic-unused".into(),
                    alipay_public_key_file: "/synthetic-unused".into(),
                },
                signing,
                verifying,
                http: reqwest::Client::new(),
            },
            provider,
        )
    }
    fn signature(key: &RsaKeyPair, bytes: &[u8]) -> String {
        let mut result = vec![0; key.public_modulus_len()];
        key.sign(
            &signature::RSA_PKCS1_SHA256,
            &SystemRandom::new(),
            bytes,
            &mut result,
        )
        .unwrap();
        STANDARD.encode(result)
    }
    #[test]
    fn rsa2_notification_binding_duplicates_and_mutation_rejection() {
        let (client, provider) = client();
        let mut fields: BTreeMap<String, String> = [
            ("app_id", "synthetic-app"),
            ("seller_id", "synthetic-seller"),
            ("sign_type", "RSA2"),
            ("charset", "utf-8"),
            ("out_trade_no", "synthetic-order"),
            ("trade_no", "synthetic-trade"),
            ("total_amount", "10.01"),
            ("trade_status", "TRADE_SUCCESS"),
        ]
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect();
        fields.insert(
            "sign".into(),
            signature(&provider, canonical(&fields, true).as_bytes()),
        );
        let form = serde_urlencoded::to_string(&fields).unwrap();
        let trade = client.verify_notification(form.as_bytes()).unwrap();
        assert!(trade.paid());
        assert_eq!(trade.amount_minor(), 1001);
        assert_eq!(trade.order_id(), "synthetic-order");
        assert!(
            client
                .verify_notification(format!("{form}&total_amount=10.01").as_bytes())
                .is_err()
        );
        assert!(
            client
                .verify_notification(form.replace("10.01", "99.99").as_bytes())
                .is_err()
        );
        assert!(
            client
                .verify_notification(
                    form.replace("synthetic-seller", "foreign-seller")
                        .as_bytes()
                )
                .is_err()
        );
        assert!(
            client
                .verify_notification(form.replace("RSA2", "RSA").as_bytes())
                .is_err()
        );
        fields.insert("total_amount".into(), "10.001".into());
        fields.insert(
            "sign".into(),
            signature(&provider, canonical(&fields, true).as_bytes()),
        );
        assert!(
            client
                .verify_notification(serde_urlencoded::to_string(&fields).unwrap().as_bytes())
                .is_err()
        );
    }
    #[test]
    fn response_uses_signed_raw_json_not_reserialized_fields() {
        let (client, provider) = client();
        let raw = r#"{ "code":"10000", "out_trade_no":"synthetic-order", "qr_code":"https://qr.alipay.com/synthetic" }"#;
        let sign = signature(&provider, raw.as_bytes());
        let packet = format!(r#"{{"alipay_trade_precreate_response":{raw},"sign":"{sign}"}}"#);
        assert_eq!(
            client
                .response("alipay.trade.precreate", packet.as_bytes())
                .unwrap()
                .get(),
            raw
        );
        assert!(
            client
                .response(
                    "alipay.trade.precreate",
                    packet.replace("10000", "20000").as_bytes()
                )
                .is_err()
        );
        assert!(
            client
                .response("alipay.trade.query", packet.as_bytes())
                .is_err()
        );
        assert!(
            client
                .response(
                    "alipay.trade.precreate",
                    packet.replace("{ \"code\"", "{\"code\"").as_bytes()
                )
                .is_err()
        );
        let signed = client.sign(b"synthetic request").unwrap();
        signature::UnparsedPublicKey::new(
            &signature::RSA_PKCS1_2048_8192_SHA256,
            client.signing.public_key().as_ref(),
        )
        .verify(b"synthetic request", &STANDARD.decode(signed).unwrap())
        .unwrap();
    }

    #[test]
    fn refund_requires_exact_success_and_all_four_bindings() {
        let (client, provider) = client();
        let check = |status: &str, amount: &str, id: &str| {
            let raw = format!(
                r#"{{"code":"10000","out_trade_no":"synthetic-order","trade_no":"synthetic-trade","out_request_no":"{id}","refund_amount":"{amount}","refund_status":"{status}"}}"#
            );
            let sign = signature(&provider, raw.as_bytes());
            let packet = format!(
                r#"{{"alipay_trade_fastpay_refund_query_response":{raw},"sign":"{sign}"}}"#
            );
            let verified = client
                .response("alipay.trade.fastpay.refund.query", packet.as_bytes())
                .unwrap();
            verified_refund(
                verified,
                "synthetic-order",
                "synthetic-trade",
                "synthetic-refund",
                1001,
            )
        };
        assert!(check("REFUND_SUCCESS", "10.01", "synthetic-refund").is_ok());
        assert!(check("", "10.01", "synthetic-refund").is_err());
        assert!(check("REFUND_SUCCESS", "10.02", "synthetic-refund").is_err());
        assert!(check("REFUND_SUCCESS", "10.01", "another-refund").is_err());
    }
}
