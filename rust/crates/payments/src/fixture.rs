//! Disposable RSA fixtures generated after Bazel builds, never cached credentials.
use aws_lc_rs::{
    encoding::{AsDer, Pkcs8V1Der},
    rand::SystemRandom,
    rsa::KeySize,
    signature::{self, KeyPair, RsaKeyPair},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use std::{
    collections::BTreeMap, fs::OpenOptions, io::Write, os::unix::fs::OpenOptionsExt, path::Path,
};

fn save(root: &Path, name: &str, bytes: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(root.join(name))?
        .write_all(bytes)?;
    Ok(())
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let directory = std::env::args()
        .nth(1)
        .ok_or("private fixture directory required")?;
    let root = Path::new(&directory).canonicalize()?;
    // Only dedicated OS temporary directories, never a repository/runfiles tree.
    if !root.starts_with(std::env::temp_dir().canonicalize()?) {
        return Err("fixture must be outside workspace".into());
    }
    let key = RsaKeyPair::generate(KeySize::Rsa2048)?;
    let der: Pkcs8V1Der<'_> = key.as_der()?;
    save(&root, "synthetic-private.der", der.as_ref())?;
    save(&root, "synthetic-public.der", key.public_key().as_ref())?;
    for environment in ["production", "sandbox"] {
        save(
            &root,
            &format!("{environment}.json"),
            &serde_json::to_vec(&serde_json::json!({
                "app_id":"synthetic-app", "seller_id":"synthetic-seller", "environment":environment,
                "notify_url":"https://example.invalid/payments/alipay/notify", "private_key_file":root.join("synthetic-private.der"),
                "alipay_public_key_file":root.join("synthetic-public.der")
            }))?,
        )?;
    }
    for (order, trade, amount, status, name) in [
        (
            "synthetic-order",
            "synthetic-trade",
            "10.01",
            "TRADE_SUCCESS",
            "paid",
        ),
        (
            "synthetic-order",
            "synthetic-trade",
            "10.01",
            "WAIT_BUYER_PAY",
            "pending",
        ),
        (
            "synthetic-order",
            "synthetic-trade",
            "10.02",
            "TRADE_SUCCESS",
            "wrong-amount",
        ),
        (
            "synthetic-missing",
            "synthetic-missing-trade",
            "10.01",
            "TRADE_SUCCESS",
            "missing",
        ),
        (
            "synthetic-rollback",
            "synthetic-rollback-trade",
            "10.01",
            "TRADE_SUCCESS",
            "rollback",
        ),
        (
            "synthetic-sandbox",
            "synthetic-sandbox-trade",
            "10.01",
            "TRADE_SUCCESS",
            "sandbox",
        ),
    ] {
        let mut fields: BTreeMap<String, String> = [
            ("app_id", "synthetic-app"),
            ("seller_id", "synthetic-seller"),
            ("charset", "utf-8"),
            ("out_trade_no", order),
            ("trade_no", trade),
            ("total_amount", amount),
            ("trade_status", status),
        ]
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect();
        let content = fields
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&");
        let mut signed = vec![0; key.public_modulus_len()];
        key.sign(
            &signature::RSA_PKCS1_SHA256,
            &SystemRandom::new(),
            content.as_bytes(),
            &mut signed,
        )?;
        fields.insert("sign".into(), STANDARD.encode(signed));
        fields.insert("sign_type".into(), "RSA2".into());
        save(
            &root,
            &format!("{name}.form"),
            serde_urlencoded::to_string(&fields)?.as_bytes(),
        )?;
    }
    let raw = r#"{"code":"10000","out_trade_no":"synthetic-order","trade_no":"synthetic-trade","out_request_no":"synthetic-refund","refund_amount":"6.00","refund_status":"REFUND_SUCCESS"}"#;
    let mut signed = vec![0; key.public_modulus_len()];
    key.sign(
        &signature::RSA_PKCS1_SHA256,
        &SystemRandom::new(),
        raw.as_bytes(),
        &mut signed,
    )?;
    let packet = format!(
        r#"{{"alipay_trade_fastpay_refund_query_response":{raw},"sign":"{}"}}"#,
        STANDARD.encode(signed)
    );
    save(&root, "refund.json", packet.as_bytes())?;
    Ok(())
}
