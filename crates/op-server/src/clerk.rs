//! Clerk session exchange for mobile clients.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use op_core::people::{self, Accepted};
use op_core::time::{now, to_db};
use op_core::users::{self, NewUser, User};
use op_core::{ApiError, ApiJson, ApiResult, Ctx, Role};
use rsa::BigUint;
use rsa::RsaPublicKey;
use rsa::pkcs1v15::{Signature, VerifyingKey};
use rsa::signature::Verifier;
use serde::{Deserialize, Serialize};
use sha2::Sha256;

const CLOCK_LEEWAY_S: i64 = 10;
const JWKS_CACHE: Duration = Duration::from_secs(300);

pub fn router() -> Router<Ctx> {
    Router::new().route("/api/auth/clerk", post(exchange)).route("/api/auth/bootstrap", post(bootstrap))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExchangeBody {
    token: String,
    name: Option<String>,
    email: Option<String>,
    label: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BootstrapBody {
    secret: String,
    name: Option<String>,
    email: Option<String>,
    label: Option<String>,
}

#[derive(Serialize)]
struct ExchangeResponse {
    #[serde(flatten)]
    accepted: Accepted,
    farm_ready: bool,
}

#[derive(Deserialize)]
struct Header {
    alg: String,
    kid: String,
}

#[derive(Debug, Deserialize)]
struct Claims {
    iss: String,
    sub: String,
    exp: i64,
    nbf: Option<i64>,
    iat: Option<i64>,
}

#[derive(Deserialize)]
struct Jwks {
    keys: Vec<Jwk>,
}

#[derive(Deserialize)]
struct Jwk {
    kid: String,
    kty: String,
    n: String,
    e: String,
    alg: Option<String>,
    #[serde(rename = "use")]
    use_: Option<String>,
}

struct CachedKeys {
    url: String,
    fetched: Instant,
    keys: HashMap<String, RsaPublicKey>,
}

fn cache() -> &'static Mutex<Option<CachedKeys>> {
    static CACHE: OnceLock<Mutex<Option<CachedKeys>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

async fn exchange(State(ctx): State<Ctx>, ApiJson(body): ApiJson<ExchangeBody>) -> ApiResult<Json<ExchangeResponse>> {
    let issuer = std::env::var("OP_CLERK_ISSUER").map_err(|_| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "Clerk sign-in is not configured."))?;
    let issuer = issuer.trim_end_matches('/');
    let jwks = std::env::var("OP_CLERK_JWKS_URL").unwrap_or_else(|_| format!("{issuer}/.well-known/jwks.json"));
    let claims = verify(&body.token, issuer, &jwks).await?;
    finish(&ctx, issuer, &claims.sub, body.name.as_deref(), body.email.as_deref(), body.label.as_deref(), None).await
}

async fn bootstrap(State(ctx): State<Ctx>, ApiJson(body): ApiJson<BootstrapBody>) -> ApiResult<Json<ExchangeResponse>> {
    let expected = std::env::var("OP_BOOTSTRAP_SECRET").map_err(|_| ApiError::new(StatusCode::NOT_FOUND, "Bootstrap sign-in is not configured."))?;
    if !constant_time_eq(body.secret.as_bytes(), expected.as_bytes()) {
        return Err(ApiError::unauthorized("Wrong bootstrap secret."));
    }
    finish(&ctx, "openpasture:bootstrap", "owner", body.name.as_deref(), body.email.as_deref(), body.label.as_deref(), Some(Role::Owner)).await
}

async fn finish(
    ctx: &Ctx,
    issuer: &str,
    subject: &str,
    name: Option<&str>,
    email: Option<&str>,
    label: Option<&str>,
    forced_role: Option<Role>,
) -> ApiResult<Json<ExchangeResponse>> {
    if subject.is_empty() || subject.len() > 255 {
        return Err(ApiError::unauthorized("The sign-in subject is invalid."));
    }
    let user = external_user(ctx, issuer, subject, name, email, forced_role).await?;
    let accepted = people::issue_token(ctx, user, label).await?;
    let farm_ready = ctx.store().get_farm().await?.is_some();
    Ok(Json(ExchangeResponse { accepted, farm_ready }))
}

async fn external_user(ctx: &Ctx, issuer: &str, subject: &str, name: Option<&str>, email: Option<&str>, forced_role: Option<Role>) -> ApiResult<User> {
    if let Some(id) = sqlx::query_scalar::<_, String>("SELECT user_id FROM user_external_ids WHERE issuer = ? AND subject = ?")
        .bind(issuer)
        .bind(subject)
        .fetch_optional(ctx.db())
        .await?
    {
        return users::get_user(ctx, &id).await?.ok_or_else(|| ApiError::unauthorized("This sign-in is no longer attached to a person."));
    }
    let owner_exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users WHERE role = 'owner' AND disabled_at IS NULL)").fetch_one(ctx.db()).await?;
    let role = forced_role.unwrap_or_else(|| if owner_exists { configured_new_role() } else { Role::Owner });
    let display_name = name.map(str::trim).filter(|s| !s.is_empty()).unwrap_or("Farmer");
    // Email and name are profile data only. A duplicate/invalid email must not
    // stop a valid Clerk subject from signing in.
    let first = users::create_user(ctx, NewUser { name: display_name.to_owned(), role, phone: None, email: email.map(str::to_owned) }).await;
    let mut user = match first {
        Ok(user) => user,
        Err(_) if email.is_some() => users::create_user(ctx, NewUser { name: display_name.to_owned(), role, phone: None, email: None }).await?,
        Err(error) => return Err(error),
    };
    let inserted = sqlx::query("INSERT OR IGNORE INTO user_external_ids (issuer, subject, user_id, created_at) VALUES (?, ?, ?, ?)")
        .bind(issuer)
        .bind(subject)
        .bind(&user.id)
        .bind(to_db(&now()))
        .execute(ctx.db())
        .await?
        .rows_affected();
    if inserted == 0 {
        // A concurrent first exchange won. Remove our unattached person.
        sqlx::query("DELETE FROM users WHERE id = ?").bind(&user.id).execute(ctx.db()).await?;
        let id: String =
            sqlx::query_scalar("SELECT user_id FROM user_external_ids WHERE issuer = ? AND subject = ?").bind(issuer).bind(subject).fetch_one(ctx.db()).await?;
        user = users::get_user(ctx, &id).await?.ok_or_else(|| ApiError::unauthorized("This sign-in is no longer attached to a person."))?;
    }
    people::people_changed(ctx);
    Ok(user)
}

fn configured_new_role() -> Role {
    match std::env::var("OP_CLERK_NEW_USER_ROLE").unwrap_or_else(|_| "manager".into()).to_ascii_lowercase().as_str() {
        "viewer" => Role::Viewer,
        "hand" => Role::Hand,
        "owner" => Role::Owner,
        _ => Role::Manager,
    }
}

async fn verify(token: &str, issuer: &str, jwks_url: &str) -> ApiResult<Claims> {
    let mut parts = token.split('.');
    let (Some(h), Some(p), Some(s), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
        return Err(ApiError::unauthorized("That Clerk session token is malformed."));
    };
    let header: Header = decode_json(h)?;
    if header.alg != "RS256" || header.kid.is_empty() {
        return Err(ApiError::unauthorized("That Clerk session token does not use RS256."));
    }
    let mut key = cached_key(jwks_url, &header.kid, false).await?;
    if key.is_none() {
        key = cached_key(jwks_url, &header.kid, true).await?;
    }
    let key = key.ok_or_else(|| ApiError::unauthorized("The Clerk signing key is unknown."))?;
    let sig_bytes = URL_SAFE_NO_PAD.decode(s).map_err(|_| ApiError::unauthorized("That Clerk session signature is malformed."))?;
    let sig = Signature::try_from(sig_bytes.as_slice()).map_err(|_| ApiError::unauthorized("That Clerk session signature is malformed."))?;
    VerifyingKey::<Sha256>::new(key)
        .verify(format!("{h}.{p}").as_bytes(), &sig)
        .map_err(|_| ApiError::unauthorized("That Clerk session signature is invalid."))?;
    let claims: Claims = decode_json(p)?;
    let now = chrono::Utc::now().timestamp();
    if claims.iss.trim_end_matches('/') != issuer || claims.exp < now - CLOCK_LEEWAY_S || claims.nbf.is_some_and(|v| v > now + CLOCK_LEEWAY_S) {
        return Err(ApiError::unauthorized("That Clerk session token is expired or not for this server."));
    }
    if claims.iat.is_some_and(|v| v > now + CLOCK_LEEWAY_S) {
        return Err(ApiError::unauthorized("That Clerk session token is not valid yet."));
    }
    Ok(claims)
}

fn decode_json<T: for<'de> Deserialize<'de>>(part: &str) -> ApiResult<T> {
    let bytes = URL_SAFE_NO_PAD.decode(part).map_err(|_| ApiError::unauthorized("That Clerk session token is malformed."))?;
    serde_json::from_slice(&bytes).map_err(|_| ApiError::unauthorized("That Clerk session token is malformed."))
}

async fn cached_key(url: &str, kid: &str, force: bool) -> ApiResult<Option<RsaPublicKey>> {
    if !force {
        let guard = cache().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(c) = guard.as_ref().filter(|c| c.url == url && c.fetched.elapsed() < JWKS_CACHE) {
            return Ok(c.keys.get(kid).cloned());
        }
    }
    let set: Jwks = reqwest::Client::new()
        .get(url)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|_| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "Clerk signing keys are temporarily unavailable."))?
        .error_for_status()
        .map_err(|_| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "Clerk signing keys are temporarily unavailable."))?
        .json()
        .await
        .map_err(|_| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "Clerk signing keys are invalid."))?;
    let mut keys = HashMap::new();
    for jwk in set.keys {
        if jwk.kty != "RSA" || jwk.alg.as_deref().is_some_and(|a| a != "RS256") || jwk.use_.as_deref().is_some_and(|u| u != "sig") {
            continue;
        }
        let Ok(n) = URL_SAFE_NO_PAD.decode(jwk.n) else { continue };
        let Ok(e) = URL_SAFE_NO_PAD.decode(jwk.e) else { continue };
        if let Ok(key) = RsaPublicKey::new(BigUint::from_bytes_be(&n), BigUint::from_bytes_be(&e)) {
            keys.insert(jwk.kid, key);
        }
    }
    let found = keys.get(kid).cloned();
    *cache().lock().unwrap_or_else(|e| e.into_inner()) = Some(CachedKeys { url: url.to_owned(), fetched: Instant::now(), keys });
    Ok(found)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |v, (x, y)| v | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsa::pkcs1v15::SigningKey;
    use rsa::signature::{SignatureEncoding, Signer};
    use rsa::traits::PublicKeyParts;
    use serde_json::json;

    #[tokio::test]
    async fn verifies_a_locally_signed_clerk_rs256_token() {
        let private = rsa::RsaPrivateKey::new(&mut rand::rngs::OsRng, 2048).unwrap();
        let public = private.to_public_key();
        let jwks = json!({"keys": [{
            "kid": "local-test", "kty": "RSA", "alg": "RS256", "use": "sig",
            "n": URL_SAFE_NO_PAD.encode(public.n().to_bytes_be()),
            "e": URL_SAFE_NO_PAD.encode(public.e().to_bytes_be())
        }]});
        let app = Router::new().route("/jwks", axum::routing::get(move || async move { Json(jwks) }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let now = chrono::Utc::now().timestamp();
        let header = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&json!({"alg":"RS256","kid":"local-test"})).unwrap());
        let payload =
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&json!({"iss":"https://clerk.test","sub":"user_123","iat":now,"nbf":now,"exp":now+60})).unwrap());
        let signing = format!("{header}.{payload}");
        let signature = SigningKey::<Sha256>::new(private).sign(signing.as_bytes());
        let token = format!("{signing}.{}", URL_SAFE_NO_PAD.encode(signature.to_bytes()));
        let claims = verify(&token, "https://clerk.test", &format!("http://{addr}/jwks")).await.unwrap();
        assert_eq!(claims.sub, "user_123");

        let wrong_issuer = verify(&token, "https://another.test", &format!("http://{addr}/jwks")).await.unwrap_err();
        assert_eq!(wrong_issuer.status, StatusCode::UNAUTHORIZED);
    }
}
