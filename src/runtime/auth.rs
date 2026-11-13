use std::fmt::Write;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use url::Url;

use super::ship::{CONNECT_TIMEOUT, REQUEST_TIMEOUT};
use crate::config::Auth;

const SCOPE: &str = "nikki:ingest";
const GRANT_TYPE: &str = "client_credentials";
const ASSERTION_TYPE: &str = "urn:ietf:params:oauth:client-assertion-type:jwt-bearer";
const ASSERTION_LIFETIME_SECONDS: i64 = 60;
const REFRESH_MARGIN: Duration = Duration::from_secs(120);
const MIN_CACHE: Duration = Duration::from_secs(1);
const JTI_BYTES: usize = 16;

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("the oauth key file `{path}` could not be read")]
    KeyUnreadable { path: String },
    #[error("the oauth key file `{path}` does not hold an RSA private key in PKCS#8 PEM form")]
    KeyInvalid { path: String },
    #[error("the http client could not be built: the platform tls stack is unavailable")]
    Client,
    #[error("the client assertion could not be built")]
    Assertion,
    #[error("the token request failed: {0}")]
    Transport(String),
    #[error("{}", refused_message(*.status, .code))]
    Refused { status: u16, code: Option<String> },
    #[error("the token response carried no usable token")]
    Unusable,
}

fn refused_message(status: u16, code: &Option<String>) -> String {
    match code {
        Some(code) => format!("the token endpoint answered {status} ({code})"),
        None => format!("the token endpoint answered {status}"),
    }
}

struct CachedToken {
    value: String,
    refresh_at: SystemTime,
}

pub struct TokenSource {
    client_id: String,
    key_id: String,
    issuer: String,
    token_url: Url,
    audience: String,
    key: EncodingKey,
    client: reqwest::Client,
    cached: Mutex<Option<CachedToken>>,
}

#[derive(Serialize)]
struct AssertionClaims<'a> {
    iss: &'a str,
    sub: &'a str,
    aud: &'a str,
    jti: &'a str,
    iat: i64,
    exp: i64,
}

#[derive(Deserialize)]
struct TokenBody {
    access_token: Option<String>,
    expires_in: Option<i64>,
}

#[derive(Deserialize)]
struct ErrorBody {
    error: Option<String>,
}

impl TokenSource {
    pub fn new(auth: &Auth) -> Result<TokenSource, AuthError> {
        let key = read_key(&auth.key_file)?;
        let client = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build();
        let Ok(client) = client else {
            return Err(AuthError::Client);
        };
        let source = TokenSource {
            client_id: auth.client_id.clone(),
            key_id: auth.key_id.clone(),
            issuer: auth.issuer.clone(),
            token_url: auth.token_url.clone(),
            audience: auth.audience.clone(),
            key,
            client,
            cached: Mutex::new(None),
        };
        if source.assertion(unix_now(), "probe").is_err() {
            return Err(AuthError::KeyInvalid {
                path: auth.key_file.display().to_string(),
            });
        }
        Ok(source)
    }

    pub async fn token(&self) -> Result<String, AuthError> {
        let mut cached = self.cached.lock().await;
        if let Some(token) = cached.as_ref()
            && SystemTime::now() < token.refresh_at
        {
            return Ok(token.value.clone());
        }
        let (value, expires_in) = self.request_token().await?;
        let lifetime = Duration::from_secs(expires_in)
            .saturating_sub(REFRESH_MARGIN)
            .max(MIN_CACHE);
        let Some(refresh_at) = SystemTime::now().checked_add(lifetime) else {
            return Err(AuthError::Unusable);
        };
        *cached = Some(CachedToken {
            value: value.clone(),
            refresh_at,
        });
        Ok(value)
    }

    pub async fn invalidate(&self) {
        *self.cached.lock().await = None;
    }

    async fn request_token(&self) -> Result<(String, u64), AuthError> {
        let assertion = self.assertion(unix_now(), &random_jti()?)?;
        let response = self
            .client
            .post(self.token_url.clone())
            .form(&self.token_form(&assertion))
            .send()
            .await
            .map_err(transport_error)?;
        let status = response.status().as_u16();
        let body = response.text().await.map_err(transport_error)?;
        parse_token_response(status, &body)
    }

    fn assertion(&self, now: i64, jti: &str) -> Result<String, AuthError> {
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some(self.key_id.clone());
        let claims = AssertionClaims {
            iss: &self.client_id,
            sub: &self.client_id,
            aud: &self.issuer,
            jti,
            iat: now,
            exp: now + ASSERTION_LIFETIME_SECONDS,
        };
        jsonwebtoken::encode(&header, &claims, &self.key).map_err(|_| AuthError::Assertion)
    }

    fn token_form(&self, assertion: &str) -> Vec<(&'static str, String)> {
        vec![
            ("grant_type", GRANT_TYPE.to_string()),
            ("client_id", self.client_id.clone()),
            ("client_assertion_type", ASSERTION_TYPE.to_string()),
            ("client_assertion", assertion.to_string()),
            ("resource", self.audience.clone()),
            ("scope", SCOPE.to_string()),
        ]
    }
}

fn read_key(path: &Path) -> Result<EncodingKey, AuthError> {
    let display = path.display().to_string();
    let Ok(pem) = std::fs::read(path) else {
        return Err(AuthError::KeyUnreadable { path: display });
    };
    let Ok(key) = EncodingKey::from_rsa_pem(&pem) else {
        return Err(AuthError::KeyInvalid { path: display });
    };
    Ok(key)
}

fn unix_now() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(elapsed) => i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX),
        Err(_) => 0,
    }
}

fn random_jti() -> Result<String, AuthError> {
    let mut bytes = [0u8; JTI_BYTES];
    aws_lc_rs::rand::fill(&mut bytes).map_err(|_| AuthError::Assertion)?;
    let mut jti = String::with_capacity(JTI_BYTES * 2);
    for byte in bytes {
        let _ = write!(jti, "{byte:02x}");
    }
    Ok(jti)
}

fn transport_error(error: reqwest::Error) -> AuthError {
    if error.is_timeout() {
        return AuthError::Transport("timed out".to_string());
    }
    if error.is_connect() {
        return AuthError::Transport("connection failed".to_string());
    }
    AuthError::Transport("request failed".to_string())
}

fn parse_token_response(status: u16, body: &str) -> Result<(String, u64), AuthError> {
    if status != 200 {
        let code = match serde_json::from_str::<ErrorBody>(body) {
            Ok(ErrorBody { error }) => error,
            Err(_) => None,
        };
        return Err(AuthError::Refused { status, code });
    }
    let Ok(TokenBody {
        access_token,
        expires_in,
    }) = serde_json::from_str(body)
    else {
        return Err(AuthError::Unusable);
    };
    let (Some(access_token), Some(expires_in)) = (access_token, expires_in) else {
        return Err(AuthError::Unusable);
    };
    let Ok(expires_in) = u64::try_from(expires_in) else {
        return Err(AuthError::Unusable);
    };
    if access_token.is_empty() || expires_in == 0 {
        return Err(AuthError::Unusable);
    }
    Ok((access_token, expires_in))
}

#[cfg(test)]
pub mod testing {
    use std::path::Path;

    use aws_lc_rs::encoding::AsDer;
    use aws_lc_rs::rsa::{KeyPair, KeySize};
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;

    pub fn write_rsa_key(path: &Path) {
        let pair = KeyPair::generate(KeySize::Rsa2048).expect("a test key is generated");
        let der = pair.as_der().expect("a test key is encoded");
        let mut text = String::from("-----BEGIN PRIVATE KEY-----\n");
        for line in STANDARD.encode(der.as_ref()).as_bytes().chunks(64) {
            text.push_str(std::str::from_utf8(line).expect("base64 is ascii"));
            text.push('\n');
        }
        text.push_str("-----END PRIVATE KEY-----\n");
        std::fs::write(path, text).expect("the test key file is written");
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use aws_lc_rs::encoding::AsDer;
    use aws_lc_rs::rsa::{KeyPair, KeySize};
    use aws_lc_rs::signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair as _};
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use jsonwebtoken::{DecodingKey, Validation};
    use serde_json::Value;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    fn pem(label: &str, der: &[u8]) -> String {
        let mut text = format!("-----BEGIN {label}-----\n");
        for line in STANDARD.encode(der).as_bytes().chunks(64) {
            text.push_str(std::str::from_utf8(line).unwrap());
            text.push('\n');
        }
        text.push_str(&format!("-----END {label}-----\n"));
        text
    }

    struct Fixture {
        _dir: tempdir::Dir,
        key_file: PathBuf,
        public_pem: String,
    }

    mod tempdir {
        use std::path::{Path, PathBuf};

        pub struct Dir(PathBuf);

        impl Dir {
            pub fn new(name: &str) -> Dir {
                let path = std::env::temp_dir().join(format!(
                    "nikki-auth-{name}-{}-{}",
                    std::process::id(),
                    super::unix_now()
                ));
                std::fs::create_dir_all(&path).unwrap();
                Dir(path)
            }

            pub fn path(&self) -> &Path {
                &self.0
            }
        }

        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    fn fixture(name: &str) -> Fixture {
        let dir = tempdir::Dir::new(name);
        let pair = KeyPair::generate(KeySize::Rsa2048).unwrap();
        let private = pair.as_der().unwrap();
        let public = pair.public_key().as_der().unwrap();
        let key_file = dir.path().join("key.pem");
        std::fs::write(&key_file, pem("PRIVATE KEY", private.as_ref())).unwrap();
        Fixture {
            _dir: dir,
            key_file,
            public_pem: pem("PUBLIC KEY", public.as_ref()),
        }
    }

    fn auth(key_file: PathBuf, token_url: &str) -> Auth {
        Auth {
            client_id: "nikki-test".to_string(),
            key_id: "nikki-test-2026".to_string(),
            key_file,
            issuer: "https://auth.example".to_string(),
            token_url: Url::parse(token_url).unwrap(),
            audience: "https://nikki.example/api".to_string(),
        }
    }

    fn source(fixture: &Fixture, token_url: &str) -> TokenSource {
        TokenSource::new(&auth(fixture.key_file.clone(), token_url)).unwrap()
    }

    #[test]
    fn assertion_carries_the_header_and_claims() {
        let fixture = fixture("assertion");
        let source = source(&fixture, "https://auth.example/api/oidc/token");
        let assertion = source.assertion(1_000, "abc123").unwrap();

        let header = jsonwebtoken::decode_header(&assertion).unwrap();
        assert_eq!(header.alg, Algorithm::RS256);
        assert_eq!(header.typ.as_deref(), Some("JWT"));
        assert_eq!(header.kid.as_deref(), Some("nikki-test-2026"));

        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_audience(&["https://auth.example"]);
        validation.validate_exp = false;
        let key = DecodingKey::from_rsa_pem(fixture.public_pem.as_bytes()).unwrap();
        let claims = jsonwebtoken::decode::<Value>(&assertion, &key, &validation)
            .unwrap()
            .claims;
        let claims = claims.as_object().unwrap();
        assert_eq!(claims.len(), 6);
        assert_eq!(claims["iss"], "nikki-test");
        assert_eq!(claims["sub"], "nikki-test");
        assert_eq!(claims["aud"], "https://auth.example");
        assert_eq!(claims["jti"], "abc123");
        assert_eq!(claims["iat"], 1_000);
        assert_eq!(claims["exp"], 1_060);
    }

    #[test]
    fn jti_is_32_hex_characters_and_never_repeats() {
        let first = random_jti().unwrap();
        let second = random_jti().unwrap();
        assert_eq!(first.len(), 32);
        assert!(first.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(first, second);
    }

    #[test]
    fn form_carries_exactly_the_six_fields() {
        let fixture = fixture("form");
        let source = source(&fixture, "https://auth.example/api/oidc/token");
        let form = source.token_form("the-assertion");
        let expected: Vec<(&str, String)> = vec![
            ("grant_type", "client_credentials".to_string()),
            ("client_id", "nikki-test".to_string()),
            (
                "client_assertion_type",
                "urn:ietf:params:oauth:client-assertion-type:jwt-bearer".to_string(),
            ),
            ("client_assertion", "the-assertion".to_string()),
            ("resource", "https://nikki.example/api".to_string()),
            ("scope", "nikki:ingest".to_string()),
        ];
        assert_eq!(form, expected);
    }

    #[test]
    fn a_good_response_yields_the_token_and_lifetime() {
        let body = r#"{"access_token":"tok","token_type":"bearer","expires_in":900}"#;
        assert_eq!(
            parse_token_response(200, body).unwrap(),
            ("tok".to_string(), 900)
        );
    }

    #[test]
    fn an_unusable_success_is_rejected() {
        for body in [
            r#"{"token_type":"bearer","expires_in":900}"#,
            r#"{"access_token":"","expires_in":900}"#,
            r#"{"access_token":"tok","expires_in":0}"#,
            r#"{"access_token":"tok","expires_in":-5}"#,
            r#"{"access_token":"tok"}"#,
            "not json",
        ] {
            match parse_token_response(200, body) {
                Err(AuthError::Unusable) => {}
                Ok(_) => panic!("expected Unusable, got a token"),
                Err(error) => panic!("expected Unusable, got {error:?}"),
            }
        }
    }

    #[test]
    fn a_refusal_names_the_status_and_code_only() {
        let body = r#"{"error":"invalid_client","error_description":"secret"}"#;
        let error = parse_token_response(401, body).unwrap_err();
        let text = error.to_string();
        match &error {
            AuthError::Refused { status, code } => {
                assert_eq!(*status, 401);
                assert_eq!(code.as_deref(), Some("invalid_client"));
            }
            other => panic!("expected Refused, got {other:?}"),
        }
        assert!(text.contains("401"));
        assert!(text.contains("invalid_client"));
        assert!(!text.contains("secret"));
    }

    #[test]
    fn a_gateway_page_is_a_refusal_without_a_code() {
        let error = parse_token_response(502, "<html>bad gateway</html>").unwrap_err();
        match error {
            AuthError::Refused { status, code } => {
                assert_eq!(status, 502);
                assert_eq!(code, None);
            }
            other => panic!("expected Refused, got {other:?}"),
        }
    }

    #[test]
    fn a_missing_key_file_names_the_path() {
        let dir = tempdir::Dir::new("missing");
        let path = dir.path().join("absent.pem");
        let error = TokenSource::new(&auth(path.clone(), "https://auth.example/t"))
            .err()
            .unwrap();
        match error {
            AuthError::KeyUnreadable { .. } => {}
            ref other => panic!("expected KeyUnreadable, got {other:?}"),
        }
        assert!(error.to_string().contains(&path.display().to_string()));
    }

    #[test]
    fn a_non_pem_file_is_rejected_without_leaking_its_content() {
        let dir = tempdir::Dir::new("garbage");
        let path = dir.path().join("key.pem");
        std::fs::write(&path, "hunter2-not-a-key").unwrap();
        let error = TokenSource::new(&auth(path.clone(), "https://auth.example/t"))
            .err()
            .unwrap();
        match error {
            AuthError::KeyInvalid { .. } => {}
            ref other => panic!("expected KeyInvalid, got {other:?}"),
        }
        let text = error.to_string();
        assert!(text.contains(&path.display().to_string()));
        assert!(!text.contains("hunter2"));
    }

    #[test]
    fn an_ec_key_is_rejected_without_leaking_its_content() {
        let dir = tempdir::Dir::new("ec");
        let path = dir.path().join("key.pem");
        let document = EcdsaKeyPair::generate_pkcs8(
            &ECDSA_P256_SHA256_ASN1_SIGNING,
            &aws_lc_rs::rand::SystemRandom::new(),
        )
        .unwrap();
        let content = pem("PRIVATE KEY", document.as_ref());
        std::fs::write(&path, &content).unwrap();
        let error = TokenSource::new(&auth(path.clone(), "https://auth.example/t"))
            .err()
            .unwrap();
        match error {
            AuthError::KeyInvalid { .. } => {}
            ref other => panic!("expected KeyInvalid, got {other:?}"),
        }
        let body_line = content.lines().nth(1).unwrap();
        assert!(!error.to_string().contains(body_line));
    }

    #[test]
    fn a_key_that_cannot_sign_is_rejected_at_startup() {
        let dir = tempdir::Dir::new("corrupt");
        let path = dir.path().join("key.pem");
        let pair = KeyPair::generate(KeySize::Rsa2048).unwrap();
        let mut der = pair.as_der().unwrap().as_ref().to_vec();
        let last = der.len() - 8;
        der[last] ^= 0xff;
        std::fs::write(&path, pem("PRIVATE KEY", &der)).unwrap();
        let error = TokenSource::new(&auth(path, "https://auth.example/t"))
            .err()
            .unwrap();
        match error {
            AuthError::KeyInvalid { .. } => {}
            ref other => panic!("expected KeyInvalid, got {other:?}"),
        }
    }

    struct TokenServer {
        url: String,
        requests: Arc<AtomicUsize>,
    }

    async fn serve(responses: Vec<(u16, &'static str)>) -> TokenServer {
        serve_with_location(responses, None).await
    }

    async fn serve_redirect(location: &'static str) -> TokenServer {
        serve_with_location(vec![(307, "")], Some(location)).await
    }

    async fn serve_with_location(
        responses: Vec<(u16, &'static str)>,
        location: Option<&'static str>,
    ) -> TokenServer {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/token", listener.local_addr().unwrap());
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&requests);
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let index = counter.fetch_add(1, Ordering::SeqCst);
                let (status, body) = responses[index.min(responses.len() - 1)];
                let mut received = Vec::new();
                let mut chunk = [0u8; 4096];
                while let Ok(read) = stream.read(&mut chunk).await {
                    if read == 0 {
                        break;
                    }
                    received.extend_from_slice(&chunk[..read]);
                    if request_complete(&received) {
                        break;
                    }
                }
                let location_header = match location {
                    Some(location) => format!("location: {location}\r\n"),
                    None => String::new(),
                };
                let reply = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\n{location_header}content-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(reply.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });
        TokenServer { url, requests }
    }

    fn request_complete(received: &[u8]) -> bool {
        let text = String::from_utf8_lossy(received);
        let Some((head, body)) = text.split_once("\r\n\r\n") else {
            return false;
        };
        let mut length = 0;
        for line in head.lines() {
            if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                length = value.trim().parse().unwrap_or(0);
            }
        }
        body.len() >= length
    }

    const GOOD: &str = r#"{"access_token":"tok","token_type":"bearer","expires_in":900}"#;

    #[tokio::test]
    async fn a_token_is_cached_until_invalidated() {
        let fixture = fixture("cache");
        let server = serve(vec![(200, GOOD)]).await;
        let source = source(&fixture, &server.url);

        assert_eq!(source.token().await.unwrap(), "tok");
        assert_eq!(source.token().await.unwrap(), "tok");
        assert_eq!(server.requests.load(Ordering::SeqCst), 1);

        source.invalidate().await;
        assert_eq!(source.token().await.unwrap(), "tok");
        assert_eq!(server.requests.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_failed_request_caches_nothing() {
        let fixture = fixture("failure");
        let server = serve(vec![(401, r#"{"error":"invalid_client"}"#), (200, GOOD)]).await;
        let source = source(&fixture, &server.url);

        match source.token().await {
            Err(AuthError::Refused { status, .. }) => assert_eq!(status, 401),
            Ok(_) => panic!("expected Refused, got a token"),
            Err(error) => panic!("expected Refused, got {error:?}"),
        }
        assert_eq!(source.token().await.unwrap(), "tok");
        assert_eq!(server.requests.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_redirect_is_a_refusal_and_is_never_followed() {
        let fixture = fixture("redirect");
        let target = serve(vec![(200, GOOD)]).await;
        let location: &'static str = Box::leak(target.url.clone().into_boxed_str());
        let server = serve_redirect(location).await;
        let source = source(&fixture, &server.url);

        match source.token().await {
            Err(AuthError::Refused { status, .. }) => assert_eq!(status, 307),
            Ok(_) => panic!("expected Refused, got a token"),
            Err(error) => panic!("expected Refused, got {error:?}"),
        }
        assert_eq!(target.requests.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_short_lifetime_still_caches_for_a_second() {
        let fixture = fixture("short");
        let server = serve(vec![(200, r#"{"access_token":"tok","expires_in":30}"#)]).await;
        let source = source(&fixture, &server.url);

        source.token().await.unwrap();
        source.token().await.unwrap();
        assert_eq!(server.requests.load(Ordering::SeqCst), 1);
    }
}
