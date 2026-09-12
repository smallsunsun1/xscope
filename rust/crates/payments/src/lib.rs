//! Alipay v2 RSA2 public-key mode. Crypto, PEM, form encoding and HTTP are
//! delegated to AWS-LC, pem, serde_urlencoded and reqwest. No money is moved here.
mod alipay;
pub use alipay::{Alipay, Config, VerifiedRefund, VerifiedTrade};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid payment configuration or protocol: {0}")]
    Invalid(&'static str),
    #[error("payment provider unavailable; outcome must be queried")]
    Unavailable,
    #[error("payment provider rejected request")]
    Rejected,
    #[error("payment signature verification failed")]
    Signature,
}
pub type Result<T> = std::result::Result<T, Error>;

/// Strict yuan -> integer fen. No binary floating point, exponent or rounding.
pub fn parse_amount(value: &str) -> Result<i64> {
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    if whole.is_empty()
        || whole.len() > 9
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || fraction.len() > 2
        || !fraction.bytes().all(|b| b.is_ascii_digit())
        || (value.contains('.') && fraction.is_empty())
    {
        return Err(Error::Invalid("invalid CNY decimal amount"));
    }
    let yuan: i64 = whole
        .parse()
        .map_err(|_| Error::Invalid("amount overflow"))?;
    let cents: i64 = if fraction.is_empty() {
        0
    } else {
        fraction
            .parse()
            .map_err(|_| Error::Invalid("invalid cents"))?
    };
    let amount = yuan * 100
        + if fraction.len() == 1 {
            cents * 10
        } else {
            cents
        };
    if !(1..=10_000_000_000).contains(&amount) {
        return Err(Error::Invalid("amount outside provider bounds"));
    }
    Ok(amount)
}
pub fn format_amount(minor: i64) -> Result<String> {
    if !(1..=10_000_000_000).contains(&minor) {
        return Err(Error::Invalid("amount outside provider bounds"));
    }
    Ok(format!("{}.{:02}", minor / 100, minor % 100))
}
pub(crate) fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[allow(clippy::unwrap_used)]
    fn exact_cents_without_rounding() {
        for minor in [1, 10, 99, 100, 99999, 10_000_000_000] {
            assert_eq!(parse_amount(&format_amount(minor).unwrap()).unwrap(), minor);
        }
        for invalid in [
            "",
            "0",
            "-1",
            "+1",
            "1.",
            ".1",
            "1.001",
            "1e2",
            "NaN",
            "100000000.01",
            "1.00 ",
        ] {
            assert!(parse_amount(invalid).is_err());
        }
    }
}
