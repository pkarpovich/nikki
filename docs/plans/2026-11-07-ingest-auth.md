# Ingest authentication: the daemon sends an Authelia bearer token

## Overview

The service's ingest endpoint (`POST {service_url}/api/v1/records`) is moving behind Authelia bearer tokens: the service checks an RS256 access token with audience `https://nikki.pkarpovich.space/api` and scope `nikki:ingest`, first in a soft mode (a request without a token still passes), then required. This plan makes the daemon obtain such a token with the OAuth 2.0 `client_credentials` grant authenticated by `private_key_jwt`, cache it, and send it as `Authorization: Bearer <token>` on every ingest request.

Each Mac is its own Authelia client (`nikki-<device>`), holding an RSA private key that was generated on that Mac and never leaves it. The authorization server holds only the public half under a key id (`kid`). There is no client secret anywhere.

Acceptance scenario: a Mac whose `config.toml` carries an `[auth]` section with its client id, key id, key file, token URL and audience ships every record with a valid bearer token; when Authelia is unreachable or refuses the client, records stay in the buffer and ship once a token can be had; a Mac without `[auth]` behaves exactly as today.

### The token exchange, written out

This is the whole external contract; nothing else needs to be read:

- **Client assertion:** a JWT signed RS256 with the device key. Header: `alg: RS256`, `typ: JWT`, `kid: <key_id>`. Claims: `iss` = `sub` = `<client_id>`, `aud` = `<token_url>`, `jti` = a fresh random UUID-like string (32 hex characters from a CSPRNG is enough), `iat` = now (seconds), `exp` = now + 60.
- **Token request:** `POST <token_url>`, `Content-Type: application/x-www-form-urlencoded`, fields `grant_type=client_credentials`, `client_id=<client_id>`, `client_assertion_type=urn:ietf:params:oauth:client-assertion-type:jwt-bearer`, `client_assertion=<assertion>`, `audience=<audience>`, `scope=nikki:ingest`. A fresh assertion for every request.
- **Success:** `200` with JSON `{"access_token": "...", "token_type": "bearer", "expires_in": <seconds>, ...}`. `expires_in` must be > 0 and `access_token` non-empty, otherwise it is a failure.
- **Failure:** any other status, typically with an OAuth error body `{"error": "invalid_client"}`. Only the status and the `error` code are ever logged.
- Machine tokens live 15 minutes.

### Non-goals

- No change to what is shipped, to the wire contract, to dedup keys or to the buffer.
- No key generation, rotation or Keychain storage in the daemon. The key file is created by hand (`openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048`), PKCS#8 PEM, and the daemon only reads it.
- No auth for anything other than ingest (the daemon reads nothing from the service).
- No token persistence across restarts: a fresh token is fetched after start.
- No retry loop of its own: the shipper's existing backoff is the retry.

### Rejected alternatives

- **Static bearer token per device in `config.toml`.** Rejected by the homelab's auth conventions: long-lived, no rotation, a secret in a config file.
- **`jsonwebtoken` with the `rust_crypto` backend.** Rejected: it pulls the `rsa` crate, whose timing side-channel advisory RUSTSEC-2023-0071 has no patched version. Use the `aws_lc_rs` backend.
- **A dedicated retry/refresh task.** Rejected: the token is fetched lazily by the shipper right before a POST, which already runs under backoff.

## Skills to invoke

Load each skill below with the Skill tool and follow its conventions before implementing any task in this plan.

- `rust-style` - all code under `src/` follows it.

## Code-Quality Rules (verify before marking each task complete)

From `rust-style`: `for` loops instead of iterator chains; `let ... else` for early returns; minimise `if let`, prefer `match`; shadow instead of renaming; no comments; newtypes and enums over strings and bools; no wildcard `_` arm on an enum the crate owns; no `matches!`; destructure structs explicitly.

From `CLAUDE.md`: no `unsafe` outside `src/macos/`; no blanket `#[allow(dead_code)]`; every new module declared; every network call has a deadline; a provider never panics; tests inline in the file they cover.

Never log or put into an error message the private key, the client assertion or the access token.

Per-task gate, and the whole of verification inside this plan:

```
mise run check
! grep -rn 'unsafe' src --include='*.rs' | grep -v '^src/macos/'
```

Do **not** run `./scripts/acceptance.sh`, any `-- --ignored` test or the real binary, and do not add `tests/stub_server.rs` cases. A human runs acceptance after the plan.

## Context (from discovery)

- `Cargo.toml` - `reqwest` 0.13 with `default-features = false` and features `native-tls`, `http2`, `charset`, `system-proxy`; `serde`, `serde_json`, `chrono`, `thiserror`, `tokio`. No JWT crate yet.
- `src/config.rs` - `Config { service_url, device, ..., ardia: Option<Ardia>, ... }`; the TOML shape uses `deny_unknown_fields` on every section; `ConfigError::Invalid { field, reason }`; a `~/` expansion helper already exists for `claude_code` roots and import dirs - reuse it for `key_file`. `Config` literals in tests (`src/providers/mod.rs` `test_config`, `src/config.rs` tests) need the new field.
- `src/runtime/ship.rs`:
  - `HttpTransport { client: reqwest::Client, endpoint: Url }`, `HttpTransport::new(service_url)` with `CONNECT_TIMEOUT` (10 s) and `REQUEST_TIMEOUT` (30 s);
  - `impl Transport for HttpTransport { async fn post(&self, body: String) -> Result<HttpResponse, TransportError> }`;
  - `decide(status, body, batch_len)` already keeps a batch on `401`, `403`, `404`, `405`, `407` and on transport errors, and backs off;
  - tests use a `Scripted` transport.
- `src/runtime/mod.rs` - `Pipeline::open(config)` builds `Shipper::new(buffer.handle(), HttpTransport::new(service_url)?)`.
- `src/main.rs` - `--check-config` loads and validates config and exits; `run` opens the pipeline.
- `README.md` - Configuration (TOML example and field table), the wire contract's request description and its response table.

## Solution Overview

1. **Config:** an optional `[auth]` section. Absent means no `Authorization` header, exactly today's behaviour.
2. **`src/runtime/auth.rs`** (new module): a `TokenSource` that owns the key, builds client assertions, requests tokens over its own `reqwest::Client` with the same deadlines, caches the token and invalidates it on demand.
3. **Transport:** `HttpTransport` holds an `Option<TokenSource>`. Before each POST it asks for a token. A token failure becomes a `TransportError`, so the batch is kept and the shipper backs off, exactly like an unreachable service. A `401` from the service invalidates the cached token, so the next attempt fetches a fresh one.
4. **Startup:** the key file is read and parsed once when the pipeline opens, and also by `--check-config`. An unreadable or non-RSA key is a startup error naming the path, never the content.

## Technical Details

### Config

```toml
[auth]
client_id = "nikki-mbp-21"
key_id = "nikki-mbp-21-2026-11-07"
key_file = "~/Library/Application Support/nikki/oauth/key.pem"
token_url = "https://auth.pkarpovich.space/api/oidc/token"
audience = "https://nikki.pkarpovich.space/api"
```

- `Config` gains `pub auth: Option<Auth>`, `pub struct Auth { pub client_id: String, pub key_id: String, pub key_file: PathBuf, pub token_url: Url, pub audience: String }`.
- Validation (`ConfigError::Invalid` naming the field): every field present and non-empty; `key_file` expands `~/` like the other paths and must end up absolute; `token_url` parses as a URL with scheme `https` and a host.
- The scope is the constant `nikki:ingest`, not configurable.

### `src/runtime/auth.rs`

- `pub struct TokenSource` holds the client id, key id, token URL, audience, the parsed signing key (`jsonwebtoken::EncodingKey::from_rsa_pem`), a `reqwest::Client` with `CONNECT_TIMEOUT` / `REQUEST_TIMEOUT`, and a `tokio::sync::Mutex<Option<CachedToken>>` where `CachedToken { value: String, refresh_at: Instant }`.
- `pub fn new(auth: &Auth) -> Result<TokenSource, AuthError>` reads the key file. Errors: unreadable file, not an RSA PEM key. Messages name the path only.
- `pub async fn token(&self) -> Result<String, AuthError>`: returns the cached value while `Instant::now() < refresh_at`; otherwise requests a new token and caches it with `refresh_at = now + expires_in - REFRESH_MARGIN`, where `REFRESH_MARGIN` is 120 s, floored so a short `expires_in` still caches for at least 1 s. A failed request caches nothing.
- `pub async fn invalidate(&self)` drops the cached token.
- `fn assertion(&self, now: i64, jti: &str) -> Result<String, AuthError>` builds the client assertion exactly as in Overview. Taking `now` and `jti` as arguments keeps it testable.
- `fn token_form(&self, assertion: &str) -> Vec<(&str, String)>` (or similar) builds the form fields; tested for exact field names and values.
- `fn parse_token_response(status: u16, body: &str) -> Result<(String, u64), AuthError>`: success rules and failure classification as in Overview. `AuthError::Refused { status, code: Option<String> }` for an OAuth error body, `AuthError::Unusable` for a `200` without a token or with `expires_in <= 0`, `AuthError::Transport(String)` for dial/TLS/timeout.
- `jti`: 16 random bytes hex-encoded. Use a CSPRNG already in the tree if there is one (`aws-lc-rs` brings `aws_lc_rs::rand::SystemRandom` once `jsonwebtoken` uses it as its backend); otherwise read 16 bytes from `/dev/urandom`. Do not add a dependency just for this.
- `AuthError` is a `thiserror` enum; its `Display` never includes the token, the assertion or the key.

### Transport

- `HttpTransport::new(service_url, tokens: Option<TokenSource>)`.
- `post`: when `tokens` is `Some`, call `token()`. On `Err`, log once per distinct failure kind at warn (status and OAuth code only) and return `Err(TransportError(...))` without contacting the service. On `Ok`, send `Authorization: Bearer <token>`. If the service answers `401`, call `invalidate()` before returning the response, so the existing `Keep` + backoff path retries with a fresh token.
- `decide` is unchanged.

### Dependencies

- `jsonwebtoken = { version = "11.1.0", default-features = false, features = ["use_pem", "aws_lc_rs"] }` (check the current 11.x release and its feature names on crates.io before adding; this is what 11.1.0 offers).
- `reqwest`: add the `form` feature.

## What Goes Where

- **Implementation Steps:** config, the token source, the transport, wiring and `--check-config`, README. All in this repository.
- **Post-Completion:** the Authelia clients, the service's soft-then-required switch, each Mac's `[auth]` section, release, acceptance.

## Implementation Steps

### Task 1: Parse and validate `[auth]`

**Files:**
- Modify: `src/config.rs`
- Modify: `src/providers/mod.rs` (the `test_config` literal)

- [x] Add `Auth`, `Config.auth: Option<Auth>` and `FileAuth` with `deny_unknown_fields`.
- [x] Validate as in Technical Details, reusing the existing `~/` expansion.
- [x] Update every `Config` literal with `auth: None`.
- [x] Tests: a full section parses with `~/` expanded; an absent section is `None`; each missing field, an empty field, a relative `key_file`, an `http://` token URL, an unparseable token URL and an unknown key are rejected with the field named.
- [x] Run the gate.

### Task 2: The token source

**Files:**
- Create: `src/runtime/auth.rs`
- Modify: `src/runtime/mod.rs` (declare the module)
- Modify: `Cargo.toml`, `Cargo.lock`

- [ ] Add the dependencies from Technical Details.
- [ ] Implement `TokenSource`, `assertion`, the form builder, `parse_token_response`, `token`, `invalidate` and `AuthError` as specified.
- [ ] Tests. Never commit a private key, not even a test one: GitGuardian fails the PR on any PEM private key. Generate the key inside the test with `aws-lc-rs` (an RSA 2048 key pair, serialised to PKCS#8 PEM for the key file and to a public PEM for `DecodingKey`), written to a temp dir:
  - the assertion decodes with the generated public key (`jsonwebtoken::decode` with `DecodingKey::from_rsa_pem`, RS256, `aud` = token URL) and carries exactly the header and claims from Overview, with `exp - iat == 60`;
  - two assertions get different `jti`;
  - the form carries exactly the six fields, with `scope=nikki:ingest`;
  - `parse_token_response`: a good `200` yields the token and lifetime; `200` without `access_token`, with an empty one, with `expires_in: 0` and with a non-JSON body are `Unusable`; `401 {"error":"invalid_client","error_description":"secret"}` is `Refused` with code `invalid_client` and the description appears nowhere in `Display`; a `502` HTML page is `Refused` with no code;
  - `TokenSource::new` fails naming the path for a missing file, a non-PEM file and an EC key PEM, and the error text never contains the file's content;
  - caching: drive `token()` against a minimal local HTTP listener (a `tokio::net::TcpListener` in the test answering canned token responses and counting requests) - two calls within the lifetime make one request; after `invalidate()` the next call makes a new request; a failing first response caches nothing and the second call requests again. The token URL in tests may be `http://127.0.0.1:<port>` only through a test-only constructor that skips the `https` check; production config still requires `https`.
- [ ] Run the gate.

### Task 3: Bearer on ingest

**Files:**
- Modify: `src/runtime/ship.rs`
- Modify: `src/runtime/mod.rs`

- [ ] `HttpTransport::new(service_url, Option<TokenSource>)`, the `post` behaviour from Technical Details, and `Pipeline::open` building the `TokenSource` from `config.auth`.
- [ ] Tests against a local listener as in Task 2 (one listener can serve both the token endpoint and the ingest path):
  - with auth, the ingest request carries `Authorization: Bearer <token>` and the token endpoint is hit once for two posts;
  - without auth, the ingest request has no `Authorization` header (existing behaviour);
  - a token endpoint failure makes `post` return `Err` and the ingest path is never hit;
  - an ingest `401` invalidates the token: the next `post` fetches a new one;
  - the existing `decide` tests pass unchanged.
- [ ] Run the gate.

### Task 4: Startup and `--check-config`

**Files:**
- Modify: `src/main.rs`
- Modify: `src/runtime/mod.rs` (if the error type needs a variant)

- [ ] An unusable key is a startup error from `Pipeline::open`, logged like the other startup errors.
- [ ] `--check-config` also builds the `TokenSource` (reads and parses the key, makes no network call) and fails on an unusable key. The startup log line names `auth=<client_id>` or `auth=off`.
- [ ] Tests for the `Pipeline::open` failure path with a bad key file, through the entry point the daemon uses.
- [ ] Run the gate.

### Task 5: Verify acceptance criteria

- [ ] With `[auth]`: every ingest carries a bearer; token failures keep records buffered. Without `[auth]`: byte-for-byte today's requests.
- [ ] Grep the diff: no `tracing` call or error `Display` includes the token, the assertion or key bytes.
- [ ] Run the gate. Nothing else.

### Task 6: [Final] Update documentation

- [ ] `README.md`:
  - Configuration: the `[auth]` example and its five field-table rows, and how the key is made (`openssl genpkey ...`, `0600`, outside any repository) and that only the public half and key id go to the authorization server.
  - The wire contract's request: the optional `Authorization: Bearer` header, scope `nikki:ingest`, and that `401`/`403` keep the batch (already in the response table) while a `401` also refreshes the token.
  - Known limitations: a token is fetched after every start; Authelia down means the buffer grows until it is back.
- [ ] Move this plan to `docs/plans/completed/`.

## Post-Completion

*Items requiring manual intervention or external systems - no checkboxes, informational only.*

- Authelia PR registering `nikki-mbp-21` and `nikki-mba-22` (public keys already generated on each Mac under `~/Library/Application Support/nikki/oauth/key.pem`).
- Service: soft mode on `POST /api/v1/records` first, then required once both Macs send tokens.
- Release, add `[auth]` on each Mac, `nikki --check-config`, upgrade, check the service log shows authenticated ingest from both devices.
- Run `./scripts/acceptance.sh`.
