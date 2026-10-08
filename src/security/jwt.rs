//! Short-lived access tokens for machines: a JWT (HS256) a hub signs after a machine shows its refresh token. A machine's long-lived login is
//! the refresh token (kept only as a hash on the hub); what travels on every connection is this token, which stops working after
//! `ACCESS_SECS` by itself. The signing key is made when the program starts, so a hub restart ends every access token and each machine simply
//! asks for a new one.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::sync::LazyLock;

/// How long an access token lives.
pub const ACCESS_SECS: i64 = 15 * 60;

static KEY: LazyLock<[u8; 32]> = LazyLock::new(|| {
    let mut k = [0u8; 32];
    getrandom::fill(&mut k).expect("the system has a random source");
    k
});

/// What an access token says: which account's machine it is, and when it was made and runs out (seconds).
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct Claims {
    pub sub: String,
    pub tid: String,
    pub iat: i64,
    pub exp: i64,
}

fn mac(data: &str) -> Hmac<Sha256> {
    let mut m =
        <Hmac<Sha256> as KeyInit>::new_from_slice(&*KEY).expect("hmac takes any key length");
    m.update(data.as_bytes());
    m
}

/// A signed access token for `node` of account `tenant`, valid from `now_s` for `ACCESS_SECS`.
pub fn sign(tenant: &str, node: &str, now_s: i64) -> String {
    let head = B64.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
    let body = B64.encode(
        serde_json::to_vec(&Claims {
            sub: node.into(),
            tid: tenant.into(),
            iat: now_s,
            exp: now_s + ACCESS_SECS,
        })
        .expect("plain data"),
    );
    let signed = format!("{head}.{body}");
    let sig = B64.encode(mac(&signed).finalize().into_bytes());
    format!("{signed}.{sig}")
}

/// The claims of a token this program signed that has not run out, else None. The signature is compared in constant time.
pub fn verify(token: &str, now_s: i64) -> Option<Claims> {
    let mut parts = token.split('.');
    let (head, body, sig) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || head != B64.encode(br#"{"alg":"HS256","typ":"JWT"}"#) {
        return None;
    }
    let want = B64.decode(sig).ok()?;
    mac(&format!("{head}.{body}")).verify_slice(&want).ok()?;
    let claims: Claims = serde_json::from_slice(&B64.decode(body).ok()?).ok()?;
    (claims.exp > now_s && claims.iat <= now_s + 60).then_some(claims)
}

/// Whether text has the shape of a JWT (three dot-separated parts), as opposed to a refresh token.
pub fn looks_like_jwt(token: &str) -> bool {
    token.starts_with("eyJ") && token.matches('.').count() == 2
}

/// When a token made by `sign` runs out, read without checking it (a client uses this to know when to ask for a new one).
pub fn expires_at(token: &str) -> Option<i64> {
    let body = token.split('.').nth(1)?;
    serde_json::from_slice::<Claims>(&B64.decode(body).ok()?)
        .ok()
        .map(|c| c.exp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_signed_token_verifies_until_it_runs_out_and_never_when_changed() {
        let t = sign("acct", "mac", 1000);
        assert!(looks_like_jwt(&t));
        let c = verify(&t, 1100).expect("valid");
        assert_eq!((c.sub.as_str(), c.tid.as_str()), ("mac", "acct"));
        assert_eq!(expires_at(&t), Some(1000 + ACCESS_SECS));
        assert!(verify(&t, 1000 + ACCESS_SECS).is_none(), "expired");
        // Another machine's name put in the middle part: the signature no longer matches.
        let other = sign("acct", "evil", 1000);
        let forged = format!(
            "{}.{}.{}",
            t.split('.').next().unwrap(),
            other.split('.').nth(1).unwrap(),
            t.split('.').nth(2).unwrap()
        );
        assert!(verify(&forged, 1100).is_none());
        assert!(verify("ccn1.abc", 1100).is_none());
        assert!(!looks_like_jwt("ccn1.abc"));
        // "alg":"none" style tokens are refused: only the one header this program writes is accepted.
        let none = format!(
            "{}.{}.",
            B64.encode(br#"{"alg":"none"}"#),
            t.split('.').nth(1).unwrap()
        );
        assert!(verify(&none, 1100).is_none());
    }
}
