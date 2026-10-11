//! Fixed-origin, bounded transport for authentication and read-only mail APIs.

use std::net::IpAddr;
use std::time::{Duration, Instant, SystemTime};

use clap::ValueEnum;
use reqwest::header::{CONTENT_TYPE, HeaderValue, RETRY_AFTER};
use reqwest::{Client, RequestBuilder, Response, StatusCode, Url};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::models::{Content, Directory, Letter, Mailbox, MailboxKind, Tenant};
use crate::secret::Secret;

const TOKEN_PATH: &str = "core/latest/token";
const TENANTS_PATH: &str = "core/latest/tenants";
const LETTER_TYPES: [&str; 6] = [
    "CLASSIC_LETTER",
    "SMART_LETTER",
    "SMART_LETTER_ANSWER",
    "SIMPLE_SHORT_MESSAGE",
    "INCAMAIL",
    "SECURESEND",
];
const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Environment {
    Production,
    Sandbox,
}

impl Environment {
    pub fn base_url(self) -> &'static str {
        match self {
            Self::Production => "https://api.epost.ch/",
            Self::Sandbox => "https://api-test.klara-epost.tech/",
        }
    }
}

#[derive(Clone, Debug)]
pub struct ClientOptions {
    pub timeout_secs: u64,
    pub max_retries: u32,
    pub max_response_bytes: usize,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            timeout_secs: 30,
            max_retries: 2,
            max_response_bytes: 32 * 1024 * 1024,
        }
    }
}

pub struct ApiClient {
    http: Client,
    base_url: Url,
    mailbox: Option<Mailbox>,
    options: ClientOptions,
    auth: AuthState,
}

enum AuthState {
    None,
    Bearer(TokenState),
    ApiKey(Secret),
}

struct TokenState {
    access: Secret,
    refresh: Option<Secret>,
    expires_at: Option<Instant>,
    refresh_expires_at: Option<Instant>,
}

// Never derive Debug or Serialize for authentication response bodies.
#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(rename = "refreshToken")]
    refresh_token: Option<String>,
    expires_in: Option<i64>,
    refresh_expires_in: Option<i64>,
    token_type: Option<String>,
}

impl ApiClient {
    pub fn new(
        environment: Environment,
        mailbox: Option<Mailbox>,
        options: ClientOptions,
    ) -> Result<Self> {
        let base_url = Url::parse(environment.base_url())
            .map_err(|_| Error::Config("invalid provider origin"))?;
        Self::build(base_url, mailbox, options)
    }

    /// Test transport accepts literal loopback addresses only; no DNS fallback.
    pub fn for_test(
        base_url: &str,
        mailbox: Option<Mailbox>,
        options: ClientOptions,
    ) -> Result<Self> {
        let base_url = Url::parse(base_url).map_err(|_| Error::Config("invalid test origin"))?;
        let host = base_url
            .host_str()
            .ok_or(Error::Config("test origin must be loopback"))?
            .trim_start_matches('[')
            .trim_end_matches(']');
        let address: IpAddr = host
            .parse()
            .map_err(|_| Error::Config("test origin must be a literal loopback address"))?;
        if !address.is_loopback()
            || !matches!(base_url.scheme(), "http" | "https")
            || !base_url.username().is_empty()
            || base_url.password().is_some()
            || base_url.query().is_some()
            || base_url.fragment().is_some()
            || base_url.path() != "/"
        {
            return Err(Error::Config("test origin must be a clean loopback origin"));
        }
        Self::build(base_url, mailbox, options)
    }

    fn build(base_url: Url, mailbox: Option<Mailbox>, options: ClientOptions) -> Result<Self> {
        if options.timeout_secs == 0
            || options.timeout_secs > 120
            || options.max_retries > 5
            || options.max_response_bytes == 0
        {
            return Err(Error::Config(
                "invalid timeout, retry or response-size bound",
            ));
        }
        if mailbox.as_ref().is_some_and(|m| {
            m.tenant_id.trim().is_empty() || m.tenant_id.chars().any(char::is_control)
        }) {
            return Err(Error::Config("explicit tenant ID is required"));
        }
        let http = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .timeout(Duration::from_secs(options.timeout_secs))
            .connect_timeout(Duration::from_secs(options.timeout_secs.min(10)))
            .build()
            .map_err(|_| Error::Network)?;
        Ok(Self {
            http,
            base_url,
            mailbox,
            options,
            auth: AuthState::None,
        })
    }

    pub fn set_bearer(&mut self, token: Secret) -> Result<()> {
        self.mailbox()?;
        validate_secret(&token)?;
        self.auth = AuthState::Bearer(TokenState {
            access: token,
            refresh: None,
            expires_at: None,
            refresh_expires_at: None,
        });
        Ok(())
    }

    pub fn set_api_key(&mut self, token: Secret) -> Result<()> {
        if self.mailbox()?.kind != MailboxKind::Business {
            return Err(Error::Config(
                "API-key authentication requires a business mailbox",
            ));
        }
        validate_secret(&token)?;
        self.auth = AuthState::ApiKey(token);
        Ok(())
    }

    pub async fn tenants_password(
        &mut self,
        username: &str,
        password: &Secret,
    ) -> Result<Vec<Tenant>> {
        validate_credentials(username, password)?;
        let request = self
            .http
            .post(self.endpoint(TENANTS_PATH)?)
            .form(&[("username", username), ("password", password.expose())]);
        let response = self.execute(request, false).await?;
        let tenants = self.json(response).await?;
        validate_tenants(tenants)
    }

    pub async fn tenants_bearer(&mut self, token: &Secret) -> Result<Vec<Tenant>> {
        validate_secret(token)?;
        let request = self
            .http
            .post(self.endpoint(TENANTS_PATH)?)
            .form(&[("access_token", token.expose())]);
        let response = self.execute(request, false).await?;
        let tenants = self.json(response).await?;
        validate_tenants(tenants)
    }

    pub async fn login(&mut self, username: &str, password: &Secret) -> Result<()> {
        validate_credentials(username, password)?;
        let mailbox = self.mailbox()?;
        let company = mailbox.company_id.to_string();
        let request = self.http.post(self.endpoint(TOKEN_PATH)?).form(&[
            ("grant_type", "password"),
            ("username", username),
            ("password", password.expose()),
            ("tenant_id", mailbox.tenant_id.as_str()),
            ("company_id", company.as_str()),
        ]);
        let response = self.execute(request, false).await?;
        let wire = self.json(response).await?;
        self.auth = AuthState::Bearer(token_state(wire)?);
        Ok(())
    }

    pub async fn inbox_page(&mut self, limit: u32, offset: u32) -> Result<Vec<Letter>> {
        validate_page(limit, offset)?;
        let business = self.mailbox()?.kind.as_bool().to_string();
        let mut query = vec![
            ("letter-folder", "INBOX_FOLDER".to_owned()),
            ("is-business-tenant", business),
            ("read-status", "ALL".to_owned()),
            ("limit", limit.to_string()),
            ("offset", offset.to_string()),
        ];
        query.extend(LETTER_TYPES.map(|value| ("letter-types", value.to_owned())));
        let response = self.get(self.endpoint("epost/v2/letters")?, &query).await?;
        self.json(response).await
    }

    pub async fn archive_page(
        &mut self,
        directory_id: Option<&str>,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<Letter>> {
        validate_page(limit, offset)?;
        let mut query = vec![("limit", limit.to_string()), ("offset", offset.to_string())];
        if let Some(id) = directory_id {
            validate_id(id)?;
            query.push(("directory-id", id.to_owned()));
        }
        let response = self
            .get(self.endpoint("epost/v2/archives/letters")?, &query)
            .await?;
        self.json(response).await
    }

    pub async fn directories(&mut self) -> Result<Vec<Directory>> {
        let response = self
            .get(self.endpoint("epost/v2/archives/directories")?, &[])
            .await?;
        self.json(response).await
    }

    pub async fn letter(&mut self, id: &str) -> Result<Letter> {
        let response = self.get(self.letter_endpoint(id, false)?, &[]).await?;
        self.json(response).await
    }

    pub async fn content(&mut self, id: &str) -> Result<Content> {
        let response = self.get(self.letter_endpoint(id, true)?, &[]).await?;
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let bytes = self.bytes(response).await?;
        Ok(Content {
            bytes,
            content_type,
        })
    }

    fn mailbox(&self) -> Result<&Mailbox> {
        self.mailbox
            .as_ref()
            .ok_or(Error::Config("explicit mailbox selection is required"))
    }

    fn endpoint(&self, path: &str) -> Result<Url> {
        self.base_url
            .join(path)
            .map_err(|_| Error::Config("invalid endpoint"))
    }

    fn letter_endpoint(&self, id: &str, content: bool) -> Result<Url> {
        validate_id(id)?;
        let mut url = self.endpoint("epost/v2/letters/")?;
        {
            let mut segments = url
                .path_segments_mut()
                .map_err(|_| Error::Config("invalid endpoint"))?;
            segments.pop_if_empty().push(id);
            if content {
                segments.push("content");
            }
        }
        Ok(url)
    }

    async fn get(&mut self, url: Url, query: &[(&str, String)]) -> Result<Response> {
        self.mailbox()?;
        self.ensure_fresh().await?;
        let request = self.authenticated(self.http.get(url.clone()).query(query))?;
        match self.execute(request, true).await {
            Err(Error::Auth) if self.has_refresh() => {
                self.refresh().await?;
                let request = self.authenticated(self.http.get(url).query(query))?;
                // Exactly one refresh/replay, even if the new token receives 401.
                self.execute(request, true).await
            }
            result => result,
        }
    }

    fn authenticated(&self, request: RequestBuilder) -> Result<RequestBuilder> {
        match &self.auth {
            AuthState::None => Err(Error::Auth),
            AuthState::Bearer(token) => Ok(request.bearer_auth(token.access.expose())),
            AuthState::ApiKey(key) => Ok(request.header("X-API-KEY", key.expose())),
        }
    }

    fn has_refresh(&self) -> bool {
        matches!(&self.auth, AuthState::Bearer(token) if token.refresh.is_some())
    }

    async fn ensure_fresh(&mut self) -> Result<()> {
        if matches!(&self.auth, AuthState::Bearer(token)
            if token.expires_at.is_some_and(|deadline| Instant::now() >= deadline))
        {
            self.refresh().await?;
        }
        Ok(())
    }

    async fn refresh(&mut self) -> Result<()> {
        let token = match &self.auth {
            AuthState::Bearer(token) => token,
            _ => return Err(Error::Auth),
        };
        if token
            .refresh_expires_at
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(Error::Auth);
        }
        let refresh = token.refresh.as_ref().ok_or(Error::Auth)?.clone();
        let previous_refresh_expiry = token.refresh_expires_at;
        let request = self.http.post(self.endpoint(TOKEN_PATH)?).form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh.expose()),
        ]);
        let response = self.execute(request, false).await?;
        let wire: TokenResponse = self.json(response).await?;
        let refresh_expiry_provided = wire.refresh_expires_in.is_some();
        // Replace atomically; a rotated refresh token supersedes the prior one.
        let mut replacement = token_state(wire)?;
        if replacement.refresh.is_none() {
            replacement.refresh = Some(refresh);
            if !refresh_expiry_provided {
                replacement.refresh_expires_at = previous_refresh_expiry;
            }
        }
        self.auth = AuthState::Bearer(replacement);
        Ok(())
    }

    async fn execute(&self, request: RequestBuilder, retry: bool) -> Result<Response> {
        for attempt in 0..=self.options.max_retries {
            let response = request
                .try_clone()
                .ok_or(Error::Config("request cannot be replayed"))?
                .send()
                .await
                .map_err(|_| Error::Network)?;
            let status = response.status();
            if status == StatusCode::OK {
                return Ok(response);
            }
            if status == StatusCode::UNAUTHORIZED {
                return Err(Error::Auth);
            }
            if retry
                && attempt < self.options.max_retries
                && matches!(status.as_u16(), 429 | 502 | 503 | 504)
            {
                let delay = retry_delay(&response, attempt)?;
                drop(response);
                tokio::time::sleep(delay).await;
                continue;
            }
            // Provider response bodies, URLs and request headers never enter errors.
            return Err(Error::Http(status.as_u16()));
        }
        Err(Error::Network)
    }

    async fn bytes(&self, mut response: Response) -> Result<Vec<u8>> {
        if response
            .content_length()
            .is_some_and(|size| size > self.options.max_response_bytes as u64)
        {
            return Err(Error::InvalidResponse(
                "response exceeds configured size bound",
            ));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| Error::Network)? {
            if chunk.len() > self.options.max_response_bytes.saturating_sub(bytes.len()) {
                return Err(Error::InvalidResponse(
                    "response exceeds configured size bound",
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }

    async fn json<T: DeserializeOwned>(&self, response: Response) -> Result<T> {
        let bytes = self.bytes(response).await?;
        serde_json::from_slice(&bytes).map_err(|_| Error::InvalidResponse("invalid JSON response"))
    }
}

fn validate_secret(secret: &Secret) -> Result<()> {
    if secret.expose().trim().is_empty() || HeaderValue::from_str(secret.expose()).is_err() {
        return Err(Error::Auth);
    }
    Ok(())
}

fn validate_credentials(username: &str, password: &Secret) -> Result<()> {
    if username.trim().is_empty() || password.expose().is_empty() {
        return Err(Error::Auth);
    }
    Ok(())
}

fn validate_tenants(tenants: Vec<Tenant>) -> Result<Vec<Tenant>> {
    if tenants
        .iter()
        .any(|tenant| tenant.tenant_id.trim().is_empty())
    {
        return Err(Error::InvalidResponse(
            "tenant discovery returned an empty tenant ID",
        ));
    }
    Ok(tenants)
}

fn validate_page(limit: u32, offset: u32) -> Result<()> {
    if !(1..=1000).contains(&limit) || offset > i32::MAX as u32 {
        return Err(Error::Config(
            "limit must be 1..1000 and offset must fit a nonnegative int32",
        ));
    }
    Ok(())
}

fn validate_id(id: &str) -> Result<()> {
    if id.trim().is_empty() || id == "." || id == ".." || id.chars().any(char::is_control) {
        return Err(Error::Config(
            "a nonempty opaque ID without control characters is required",
        ));
    }
    Ok(())
}

fn token_state(wire: TokenResponse) -> Result<TokenState> {
    let access = Secret::new(wire.access_token);
    validate_secret(&access)?;
    if wire
        .token_type
        .is_some_and(|kind| !kind.eq_ignore_ascii_case("bearer"))
    {
        return Err(Error::InvalidResponse("unsupported token type"));
    }
    let refresh = wire.refresh_token.map(Secret::new);
    if let Some(secret) = &refresh {
        validate_secret(secret)?;
    }
    let now = Instant::now();
    let expires_at = expiry(now, wire.expires_in)?;
    let refresh_expires_at = expiry(now, wire.refresh_expires_in)?;
    Ok(TokenState {
        access,
        refresh,
        expires_at,
        refresh_expires_at,
    })
}

fn expiry(now: Instant, seconds: Option<i64>) -> Result<Option<Instant>> {
    seconds
        .map(|value| {
            let value =
                u64::try_from(value).map_err(|_| Error::InvalidResponse("invalid token expiry"))?;
            now.checked_add(Duration::from_secs(value))
                .ok_or(Error::InvalidResponse("invalid token expiry"))
        })
        .transpose()
}

fn retry_delay(response: &Response, attempt: u32) -> Result<Duration> {
    let delay = match response.headers().get(RETRY_AFTER) {
        Some(value) => {
            let value = value
                .to_str()
                .map_err(|_| Error::Http(response.status().as_u16()))?;
            if let Ok(seconds) = value.parse::<u64>() {
                Duration::from_secs(seconds)
            } else {
                let when = httpdate::parse_http_date(value)
                    .map_err(|_| Error::Http(response.status().as_u16()))?;
                when.duration_since(SystemTime::now()).unwrap_or_default()
            }
        }
        None => Duration::from_millis(100 * 2u64.pow(attempt)),
    };
    if delay > MAX_RETRY_DELAY {
        // Refuse an early retry instead of truncating the provider's requested delay.
        return Err(Error::Http(response.status().as_u16()));
    }
    Ok(delay)
}
