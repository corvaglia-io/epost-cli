//! Bounded, fixed-origin Paperless-ngx transport.
//!
//! Uploads and tag creation are deliberately never retried: an interrupted
//! response may still have queued a document or created a tag. The caller must
//! reconcile its durable delivery state before attempting another upload.

use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;
use std::time::{Duration, SystemTime};

use reqwest::header::{AUTHORIZATION, HeaderValue, LOCATION, RETRY_AFTER};
use reqwest::{Client, RequestBuilder, Response, StatusCode, Url};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{Error, Result};
use crate::secret::Secret;

const HARD_MAX_BYTES: usize = 64 * 1024 * 1024;
const PAGE_SIZE: usize = 100;
const MAX_ITEMS: usize = 5_000;
const MAX_PAGES: usize = 50;
const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);

#[derive(Clone, Debug)]
pub struct PaperlessOptions {
    pub timeout_secs: u64,
    pub max_retries: u32,
    pub max_response_bytes: usize,
}

impl Default for PaperlessOptions {
    fn default() -> Self {
        Self {
            timeout_secs: 30,
            max_retries: 2,
            max_response_bytes: 32 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Tag {
    pub id: i64,
    pub name: String,
    pub is_inbox_tag: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskStatus {
    Pending,
    Success,
    Failure,
}

/// Fixed diagnostics only; authenticated task results can contain document
/// titles, filenames, paths and secrets and must never escape the parser.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskFailureCode {
    Duplicate,
    ProtectedPdf,
    Unsupported,
    ProcessingFailure,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Task {
    pub task_id: String,
    pub status: TaskStatus,
    pub document_id: Option<i64>,
    pub failure_code: Option<TaskFailureCode>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Document {
    pub id: i64,
    pub tags: Vec<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DocumentType {
    pub id: i64,
    pub name: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CustomField {
    pub id: i64,
    pub name: String,
    pub data_type: String,
}

/// Optional upload metadata. Receipt time belongs in a Text custom field;
/// this type intentionally has no document issue/creation date override.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UploadMetadata {
    pub title: Option<String>,
    pub tag_ids: Vec<i64>,
    pub document_type: Option<i64>,
    pub custom_fields: BTreeMap<i64, Value>,
}

// Do not derive Debug: reqwest's client stores authentication headers.
pub struct PaperlessClient {
    http: Client,
    base_url: Url,
    options: PaperlessOptions,
}

/// Normalize an installation URL, including an optional installation path.
/// Plain HTTP is reserved for a literal loopback address in synthetic tests.
pub fn normalize_url(base_url: &str, allow_loopback: bool) -> Result<String> {
    if base_url.len() > 2_048
        || base_url.chars().any(char::is_control)
        || base_url.contains('\\')
        || base_url.trim() != base_url
    {
        return Err(Error::Config("invalid Paperless installation URL"));
    }
    let mut url =
        Url::parse(base_url).map_err(|_| Error::Config("invalid Paperless installation URL"))?;
    let literal_loopback = is_literal_loopback(&url);
    let has_userinfo = base_url
        .split_once("://")
        .map(|(_, remainder)| remainder.split(['/', '?', '#']).next().unwrap_or_default())
        .is_some_and(|authority| authority.contains('@'));
    if url.host_str().is_none()
        || !(url.scheme() == "https"
            || (allow_loopback && url.scheme() == "http" && literal_loopback))
        || !url.username().is_empty()
        || url.password().is_some()
        || has_userinfo
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(Error::Config(
            "Paperless requires an HTTPS installation URL without credentials, query or fragment",
        ));
    }
    let lower_path = url.path().to_ascii_lowercase();
    if ["%2f", "%5c", "%00", "%0a", "%0d"]
        .iter()
        .any(|encoded| lower_path.contains(encoded))
    {
        return Err(Error::Config("invalid Paperless installation path"));
    }
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }
    Ok(url.to_string())
}

fn is_literal_loopback(url: &Url) -> bool {
    url.host_str()
        .map(|host| host.trim_start_matches('[').trim_end_matches(']'))
        .and_then(|host| host.parse::<IpAddr>().ok())
        .is_some_and(|address| address.is_loopback())
}

impl PaperlessClient {
    pub fn new(base_url: &str, token: Secret, options: PaperlessOptions) -> Result<Self> {
        Self::build(base_url, token, options, false)
    }

    /// Test transport never permits DNS or a non-loopback destination.
    pub fn for_test(base_url: &str, token: Secret, options: PaperlessOptions) -> Result<Self> {
        let url = Url::parse(base_url).map_err(|_| Error::Config("invalid Paperless test URL"))?;
        if !is_literal_loopback(&url) {
            return Err(Error::Config(
                "Paperless test URL must use a literal loopback address",
            ));
        }
        Self::build(base_url, token, options, true)
    }

    fn build(
        base_url: &str,
        token: Secret,
        options: PaperlessOptions,
        allow_loopback: bool,
    ) -> Result<Self> {
        if options.timeout_secs == 0
            || options.timeout_secs > 120
            || options.max_retries > 5
            || options.max_response_bytes == 0
            || options.max_response_bytes > HARD_MAX_BYTES
        {
            return Err(Error::Config(
                "invalid Paperless timeout, retry or response-size bound",
            ));
        }
        if token.expose().is_empty()
            || token.expose().len() > 16_384
            || token.expose().chars().any(|c| !c.is_ascii_graphic())
        {
            return Err(Error::Auth);
        }
        let mut authorization =
            HeaderValue::from_str(&format!("Token {}", token.expose())).map_err(|_| Error::Auth)?;
        authorization.set_sensitive(true);
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(AUTHORIZATION, authorization);
        let http = Client::builder()
            .default_headers(headers)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .timeout(Duration::from_secs(options.timeout_secs))
            .connect_timeout(Duration::from_secs(options.timeout_secs.min(10)))
            .build()
            .map_err(|_| Error::Network)?;
        let canonical = normalize_url(base_url, allow_loopback)?;
        let base_url = Url::parse(&canonical)
            .map_err(|_| Error::Config("invalid Paperless installation URL"))?;
        Ok(Self {
            http,
            base_url,
            options,
        })
    }

    pub fn canonical_url(&self) -> &str {
        self.base_url.as_str()
    }

    pub async fn tags(&self) -> Result<Vec<Tag>> {
        let entries = self.list("api/tags/", &[("ordering", "id".into())]).await?;
        let mut ids = BTreeSet::new();
        entries
            .into_iter()
            .map(|value| {
                let tag = parse_tag(value)?;
                if !ids.insert(tag.id) {
                    return Err(Error::InvalidResponse("duplicate Paperless tag ID"));
                }
                Ok(tag)
            })
            .collect()
    }

    pub async fn tag(&self, id: i64) -> Result<Tag> {
        validate_document_id(id)?;
        let response = self.get(&format!("api/tags/{id}/"), &[]).await?;
        let tag = parse_tag(self.json(response).await?)?;
        if tag.id != id {
            return Err(Error::InvalidResponse("Paperless tag ID does not match"));
        }
        Ok(tag)
    }

    pub async fn create_tag(&self, name: &str, inbox: bool) -> Result<Tag> {
        validate_tag_name(name).map_err(|_| Error::Config("invalid Paperless tag name"))?;
        let request = self
            .http
            .post(self.endpoint("api/tags/")?)
            .json(&serde_json::json!({"name": name, "is_inbox_tag": inbox}));
        let response = self.post(request).await?;
        let tag = parse_tag(self.json(response).await?)?;
        if tag.name != name || tag.is_inbox_tag != inbox {
            return Err(Error::InvalidResponse(
                "created Paperless tag does not match request",
            ));
        }
        Ok(tag)
    }

    pub async fn document_types(&self) -> Result<Vec<DocumentType>> {
        let entries = self.list("api/document_types/", &[]).await?;
        let mut seen = BTreeSet::new();
        entries
            .into_iter()
            .map(|entry| {
                let item = parse_document_type(entry)?;
                if !seen.insert(item.id) {
                    return Err(Error::InvalidResponse(
                        "duplicate Paperless document type ID",
                    ));
                }
                Ok(item)
            })
            .collect()
    }

    pub async fn document_type(&self, id: i64) -> Result<DocumentType> {
        validate_document_id(id)?;
        let response = self.get(&format!("api/document_types/{id}/"), &[]).await?;
        let item = parse_document_type(self.json(response).await?)?;
        if item.id != id {
            return Err(Error::InvalidResponse(
                "Paperless document type ID does not match",
            ));
        }
        Ok(item)
    }

    pub async fn create_document_type(&self, name: &str) -> Result<DocumentType> {
        validate_tag_name(name)
            .map_err(|_| Error::Config("invalid Paperless document type name"))?;
        let response = self
            .post(
                self.http
                    .post(self.endpoint("api/document_types/")?)
                    .json(&serde_json::json!({"name":name})),
            )
            .await?;
        let item = parse_document_type(self.json(response).await?)?;
        if item.name != name {
            return Err(Error::InvalidResponse(
                "created Paperless document type does not match request",
            ));
        }
        Ok(item)
    }

    pub async fn custom_fields(&self) -> Result<Vec<CustomField>> {
        let entries = self.list("api/custom_fields/", &[]).await?;
        let mut seen = BTreeSet::new();
        entries
            .into_iter()
            .map(|entry| {
                let item = parse_custom_field(entry)?;
                if !seen.insert(item.id) {
                    return Err(Error::InvalidResponse(
                        "duplicate Paperless custom field ID",
                    ));
                }
                Ok(item)
            })
            .collect()
    }

    pub async fn custom_field(&self, id: i64) -> Result<CustomField> {
        validate_document_id(id)?;
        let response = self.get(&format!("api/custom_fields/{id}/"), &[]).await?;
        let item = parse_custom_field(self.json(response).await?)?;
        if item.id != id {
            return Err(Error::InvalidResponse(
                "Paperless custom field ID does not match",
            ));
        }
        Ok(item)
    }

    /// Create the Text datatype used by source provenance mappings.
    pub async fn create_custom_field(&self, name: &str) -> Result<CustomField> {
        validate_tag_name(name)
            .map_err(|_| Error::Config("invalid Paperless custom field name"))?;
        let response = self
            .post(
                self.http
                    .post(self.endpoint("api/custom_fields/")?)
                    .json(&serde_json::json!({"name":name,"data_type":"string"})),
            )
            .await?;
        let item = parse_custom_field(self.json(response).await?)?;
        if item.name != name || item.data_type != "string" {
            return Err(Error::InvalidResponse(
                "created Paperless custom field does not match request",
            ));
        }
        Ok(item)
    }

    pub async fn upload_pdf(&self, filename: &str, bytes: Vec<u8>, tags: &[i64]) -> Result<String> {
        self.upload_pdf_with_metadata(
            filename,
            bytes,
            &UploadMetadata {
                tag_ids: tags.to_vec(),
                ..Default::default()
            },
        )
        .await
    }

    pub fn validate_upload_metadata(
        &self,
        filename: &str,
        bytes: &[u8],
        metadata: &UploadMetadata,
    ) -> Result<()> {
        self.validate_upload(filename, bytes, &metadata.tag_ids)?;
        if metadata.document_type.is_some_and(|id| id <= 0) || metadata.custom_fields.len() > 64 {
            return Err(Error::Config("invalid Paperless upload metadata"));
        }
        if let Some(title) = &metadata.title {
            validate_upload_text(title, 4096)?;
        }
        for (id, value) in &metadata.custom_fields {
            if *id <= 0 {
                return Err(Error::Config("invalid Paperless custom field ID"));
            }
            let text = value.as_str().ok_or(Error::Config(
                "Paperless provenance custom fields must contain Text values",
            ))?;
            validate_upload_text(text, 4096)?;
        }
        Ok(())
    }

    pub async fn upload_pdf_with_metadata(
        &self,
        filename: &str,
        bytes: Vec<u8>,
        metadata: &UploadMetadata,
    ) -> Result<String> {
        self.validate_upload_metadata(filename, &bytes, metadata)?;
        let part = reqwest::multipart::Part::bytes(bytes)
            .file_name(filename.to_owned())
            .mime_str("application/pdf")
            .map_err(|_| Error::Config("invalid Paperless upload media type"))?;
        let mut form = reqwest::multipart::Form::new().part("document", part);
        for tag in metadata.tag_ids.iter().copied().collect::<BTreeSet<_>>() {
            form = form.text("tags", tag.to_string());
        }
        if let Some(title) = &metadata.title {
            form = form.text("title", title.clone());
        }
        if let Some(id) = metadata.document_type {
            form = form.text("document_type", id.to_string());
        }
        if !metadata.custom_fields.is_empty() {
            let fields = serde_json::to_string(&metadata.custom_fields)
                .map_err(|_| Error::Config("invalid Paperless custom field values"))?;
            form = form.text("custom_fields", fields);
        }
        let request = self
            .http
            .post(self.endpoint("api/documents/post_document/")?)
            .multipart(form);
        let response = self.post(request).await?;
        let value = self.json(response).await?;
        let task_id = value.as_str().ok_or(Error::InvalidResponse(
            "Paperless upload did not return a task UUID",
        ))?;
        validate_task_id(task_id)
            .map_err(|_| Error::InvalidResponse("Paperless upload did not return a task UUID"))?;
        Ok(task_id.to_owned())
    }

    /// Run local checks before persisting an upload intent. This performs no I/O.
    pub fn validate_upload(&self, filename: &str, bytes: &[u8], tags: &[i64]) -> Result<()> {
        validate_filename(filename)?;
        if bytes.len() > self.options.max_response_bytes {
            return Err(Error::Config(
                "Paperless upload exceeds configured size bound",
            ));
        }
        if !bytes.starts_with(b"%PDF-")
            || !bytes
                .iter()
                .rposition(|byte| !byte.is_ascii_whitespace())
                .is_some_and(|end| bytes[..=end].ends_with(b"%%EOF"))
        {
            return Err(Error::Config("Paperless upload must be a validated PDF"));
        }
        if tags.len() > 100 || tags.iter().any(|id| *id <= 0) {
            return Err(Error::Config("invalid Paperless upload tags"));
        }
        Ok(())
    }

    pub async fn task(&self, task_id: &str) -> Result<Option<Task>> {
        validate_task_id(task_id)?;
        let entries = self
            .list("api/tasks/", &[("task_id", task_id.into())])
            .await?;
        if entries.len() > 1 {
            return Err(Error::InvalidResponse("ambiguous Paperless task response"));
        }
        entries
            .into_iter()
            .next()
            .map(|entry| parse_task(entry, task_id))
            .transpose()
    }

    pub async fn original(&self, document_id: i64) -> Result<Vec<u8>> {
        validate_document_id(document_id)?;
        let response = self
            .get(
                &format!("api/documents/{document_id}/download/"),
                &[("original", "true".into())],
            )
            .await?;
        self.bytes(response).await
    }

    pub async fn document(&self, document_id: i64) -> Result<Document> {
        validate_document_id(document_id)?;
        let response = self
            .get(&format!("api/documents/{document_id}/"), &[])
            .await?;
        let value = self.json(response).await?;
        let id = positive_id(value.get("id"))?;
        if id != document_id {
            return Err(Error::InvalidResponse(
                "Paperless document ID does not match",
            ));
        }
        let tags = value
            .get("tags")
            .and_then(Value::as_array)
            .filter(|tags| tags.len() <= 100)
            .ok_or(Error::InvalidResponse("invalid Paperless document tags"))?;
        let mut ids = BTreeSet::new();
        for tag in tags {
            if !ids.insert(positive_id(Some(tag))?) {
                return Err(Error::InvalidResponse("duplicate Paperless document tag"));
            }
        }
        Ok(Document {
            id,
            tags: ids.into_iter().collect(),
        })
    }

    /// Find exact upload filenames for crash reconciliation. The response is
    /// still checked case-sensitively, even though the provider filter is iexact.
    pub async fn find_documents(&self, filename: &str) -> Result<Vec<i64>> {
        validate_filename(filename)?;
        let entries = self
            .list(
                "api/documents/",
                &[
                    ("original_filename__iexact", filename.into()),
                    ("ordering", "id".into()),
                ],
            )
            .await?;
        let mut ids = BTreeSet::new();
        for entry in entries {
            let id = positive_id(entry.get("id"))?;
            let candidate = entry
                .get("original_file_name")
                .and_then(Value::as_str)
                .ok_or(Error::InvalidResponse(
                    "Paperless document has no original filename",
                ))?;
            if candidate.len() > 1_024 || candidate.chars().any(char::is_control) {
                return Err(Error::InvalidResponse(
                    "invalid Paperless document filename",
                ));
            }
            if candidate == filename && !ids.insert(id) {
                return Err(Error::InvalidResponse("duplicate Paperless document ID"));
            }
        }
        Ok(ids.into_iter().collect())
    }

    fn endpoint(&self, path: &str) -> Result<Url> {
        self.base_url
            .join(path)
            .map_err(|_| Error::Config("invalid Paperless endpoint"))
    }

    async fn list(&self, path: &str, query: &[(&str, String)]) -> Result<Vec<Value>> {
        let mut result = Vec::new();
        let mut expected_count = None;
        for page in 1..=MAX_PAGES {
            let mut query = query.to_vec();
            query.push(("page", page.to_string()));
            query.push(("page_size", PAGE_SIZE.to_string()));
            let response = self.get(path, &query).await?;
            let value = self.json(response).await?;
            // Pre-v10 tasks are an unpaginated list. Permit this old contract
            // on page 1 only, with the same overall item bound.
            if let Value::Array(entries) = value {
                if page != 1 || entries.len() > MAX_ITEMS {
                    return Err(Error::InvalidResponse("Paperless list exceeds item bound"));
                }
                return Ok(entries);
            }
            let count = value
                .get("count")
                .and_then(Value::as_u64)
                .and_then(|number| usize::try_from(number).ok())
                .filter(|number| *number <= MAX_ITEMS)
                .ok_or(Error::InvalidResponse("invalid Paperless pagination count"))?;
            if expected_count.is_some_and(|previous| previous != count) {
                return Err(Error::InvalidResponse(
                    "Paperless inventory changed during pagination",
                ));
            }
            expected_count = Some(count);
            let entries = value
                .get("results")
                .and_then(Value::as_array)
                .ok_or(Error::InvalidResponse("invalid Paperless page results"))?;
            let has_next = match value.get("next") {
                Some(Value::Null) => false,
                Some(Value::String(next)) if !next.is_empty() && next.len() <= 2_048 => true,
                _ => return Err(Error::InvalidResponse("invalid Paperless next-page marker")),
            };
            if entries.len() > PAGE_SIZE
                || result.len().saturating_add(entries.len()) > count
                || (has_next && entries.is_empty())
            {
                return Err(Error::InvalidResponse(
                    "invalid Paperless pagination progress",
                ));
            }
            result.extend(entries.iter().cloned());
            if !has_next {
                if result.len() != count {
                    return Err(Error::InvalidResponse("incomplete Paperless pagination"));
                }
                return Ok(result);
            }
            if result.len() >= count {
                return Err(Error::InvalidResponse(
                    "invalid Paperless pagination continuation",
                ));
            }
            // Deliberately ignore the response URL. Only the next local page
            // number can control another request to the configured installation.
        }
        Err(Error::InvalidResponse(
            "Paperless pagination exceeds page bound",
        ))
    }

    async fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Response> {
        let request = self.http.get(self.endpoint(path)?).query(query);
        for attempt in 0..=self.options.max_retries {
            let sent = request
                .try_clone()
                .ok_or(Error::Config("Paperless GET cannot be replayed"))?
                .send()
                .await;
            let response = match sent {
                Ok(response) => response,
                Err(_) if attempt < self.options.max_retries => {
                    tokio::time::sleep(backoff(attempt)).await;
                    continue;
                }
                Err(_) => return Err(Error::Network),
            };
            if response.status() == StatusCode::OK {
                return Ok(response);
            }
            if response.status().is_redirection() {
                return Err(self.redirect_error(&response));
            }
            if matches!(response.status().as_u16(), 401 | 403) {
                return Err(Error::Auth);
            }
            if attempt < self.options.max_retries
                && matches!(response.status().as_u16(), 429 | 500 | 502 | 503 | 504)
            {
                let delay = retry_delay(&response, attempt)?;
                drop(response);
                tokio::time::sleep(delay).await;
                continue;
            }
            return Err(Error::Http(response.status().as_u16()));
        }
        Err(Error::Network)
    }

    async fn post(&self, request: RequestBuilder) -> Result<Response> {
        let response = request.send().await.map_err(|_| Error::Network)?;
        if response.status().is_redirection() {
            return Err(self.redirect_error(&response));
        }
        match response.status().as_u16() {
            200..=202 => Ok(response),
            401 | 403 => Err(Error::Auth),
            status => Err(Error::Http(status)),
        }
    }

    fn redirect_error(&self, response: &Response) -> Error {
        const UNKNOWN: &str = "the redirect was not followed; check the canonical installation URL, reverse proxy and access policy";
        const CLOUDFLARE_ACCESS: &str = "the redirect appears to require Cloudflare Access; configure authorized machine access or an internal HTTPS installation URL";
        const SIGN_IN: &str = "the redirect points to a sign-in or authentication route; a browser-login proxy may be blocking token-authenticated API access";
        const API_ROUTE: &str = "the redirect points to an API route on the same origin; check the canonical installation URL and reverse-proxy routing";

        // Location may contain a credential, document name, signed query or
        // login state. Inspect it only in memory and return fixed guidance.
        let target = response
            .headers()
            .get(LOCATION)
            .filter(|value| value.as_bytes().len() <= 2_048)
            .and_then(|value| value.to_str().ok())
            .filter(|value| {
                !value.is_empty()
                    && value.trim() == *value
                    && !value.chars().any(char::is_control)
                    && !value.contains('\\')
            })
            .and_then(|value| response.url().join(value).ok())
            .filter(|url| {
                matches!(url.scheme(), "http" | "https")
                    && url.host_str().is_some()
                    && url.username().is_empty()
                    && url.password().is_none()
            });
        let guidance = match target {
            Some(target)
                if target
                    .host_str()
                    .is_some_and(|host| host.ends_with(".cloudflareaccess.com"))
                    || target.path().starts_with("/cdn-cgi/access/") =>
            {
                CLOUDFLARE_ACCESS
            }
            Some(target)
                if target.path_segments().is_some_and(|segments| {
                    segments.into_iter().any(|segment| {
                        matches!(
                            segment.to_ascii_lowercase().as_str(),
                            "login"
                                | "signin"
                                | "sign-in"
                                | "authenticate"
                                | "authentication"
                                | "auth"
                                | "oauth"
                                | "oauth2"
                                | "authorize"
                                | "authorization"
                                | "sso"
                        )
                    })
                }) =>
            {
                SIGN_IN
            }
            Some(target)
                if target.origin() == self.base_url.origin()
                    && (target.path().starts_with("/api/")
                        || target
                            .path()
                            .starts_with(&format!("{}api/", self.base_url.path()))) =>
            {
                API_ROUTE
            }
            _ => UNKNOWN,
        };
        Error::PaperlessRedirect {
            status: response.status().as_u16(),
            guidance,
        }
    }

    async fn bytes(&self, mut response: Response) -> Result<Vec<u8>> {
        if response
            .content_length()
            .is_some_and(|size| size > self.options.max_response_bytes as u64)
        {
            return Err(Error::InvalidResponse(
                "Paperless response exceeds size bound",
            ));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| Error::Network)? {
            if chunk.len() > self.options.max_response_bytes.saturating_sub(bytes.len()) {
                return Err(Error::InvalidResponse(
                    "Paperless response exceeds size bound",
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }

    async fn json(&self, response: Response) -> Result<Value> {
        serde_json::from_slice(&self.bytes(response).await?)
            .map_err(|_| Error::InvalidResponse("invalid Paperless JSON response"))
    }
}

fn validate_upload_text(text: &str, max: usize) -> Result<()> {
    if text.trim().is_empty() || text.len() > max || text.chars().any(char::is_control) {
        return Err(Error::Config("invalid Paperless metadata text"));
    }
    Ok(())
}

fn parse_document_type(value: Value) -> Result<DocumentType> {
    let item: DocumentType = serde_json::from_value(value)
        .map_err(|_| Error::InvalidResponse("invalid Paperless document type"))?;
    if item.id <= 0 {
        return Err(Error::InvalidResponse("invalid Paperless document type ID"));
    }
    validate_tag_name(&item.name)?;
    Ok(item)
}

fn parse_custom_field(value: Value) -> Result<CustomField> {
    let item: CustomField = serde_json::from_value(value)
        .map_err(|_| Error::InvalidResponse("invalid Paperless custom field"))?;
    if item.id <= 0
        || item.data_type.is_empty()
        || item.data_type.len() > 32
        || !item
            .data_type
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_')
    {
        return Err(Error::InvalidResponse(
            "invalid Paperless custom field ID or datatype",
        ));
    }
    validate_tag_name(&item.name)?;
    Ok(item)
}

fn validate_document_id(id: i64) -> Result<()> {
    if id <= 0 {
        return Err(Error::Config("Paperless IDs must be positive integers"));
    }
    Ok(())
}

fn positive_id(value: Option<&Value>) -> Result<i64> {
    value
        .and_then(|value| {
            value
                .as_i64()
                .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
        })
        .filter(|id| *id > 0)
        .ok_or(Error::InvalidResponse(
            "invalid Paperless document or tag ID",
        ))
}

fn validate_filename(filename: &str) -> Result<()> {
    if filename.is_empty()
        || filename.len() > 255
        || matches!(filename, "." | "..")
        || filename
            .chars()
            .any(|c| c.is_control() || matches!(c, '/' | '\\' | '"'))
    {
        return Err(Error::Config("invalid Paperless upload filename"));
    }
    Ok(())
}

fn validate_tag_name(name: &str) -> Result<()> {
    if name.trim().is_empty() || name.len() > 128 || name.chars().any(char::is_control) {
        return Err(Error::InvalidResponse("invalid Paperless tag name"));
    }
    Ok(())
}

fn parse_tag(value: Value) -> Result<Tag> {
    let tag: Tag = serde_json::from_value(value)
        .map_err(|_| Error::InvalidResponse("invalid Paperless tag response"))?;
    if tag.id <= 0 {
        return Err(Error::InvalidResponse("invalid Paperless tag ID"));
    }
    validate_tag_name(&tag.name)?;
    Ok(tag)
}

fn validate_task_id(task_id: &str) -> Result<()> {
    if task_id.len() != 36
        || task_id.bytes().enumerate().any(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte != b'-'
            } else {
                !byte.is_ascii_hexdigit()
            }
        })
    {
        return Err(Error::Config("Paperless task ID must be a UUID"));
    }
    Ok(())
}

fn parse_task(value: Value, expected_task_id: &str) -> Result<Task> {
    let task_id = value
        .get("task_id")
        .and_then(Value::as_str)
        .filter(|id| *id == expected_task_id)
        .ok_or(Error::InvalidResponse("Paperless task ID does not match"))?;
    let status = value
        .get("status")
        .and_then(Value::as_str)
        .ok_or(Error::InvalidResponse("invalid Paperless task status"))?;
    let reported_failure = status.eq_ignore_ascii_case("FAILURE");
    let status = match status.to_ascii_uppercase().as_str() {
        "PENDING" | "STARTED" | "RETRY" => TaskStatus::Pending,
        "SUCCESS" => TaskStatus::Success,
        "FAILURE" | "REVOKED" => TaskStatus::Failure,
        _ => return Err(Error::InvalidResponse("unsupported Paperless task status")),
    };
    let mut document_id = match value.get("related_document_ids") {
        Some(Value::Array(ids)) if ids.is_empty() => None,
        Some(Value::Array(ids)) if ids.len() == 1 => Some(positive_id(ids.first())?),
        Some(_) => {
            return Err(Error::InvalidResponse(
                "ambiguous Paperless task document IDs",
            ));
        }
        None => match value.get("related_document") {
            None | Some(Value::Null) => None,
            value => Some(positive_id(value)?),
        },
    };
    let failure_code = if status == TaskStatus::Failure {
        let (code, duplicate_id) = if reported_failure {
            classify_task_failure(&value)
        } else {
            (TaskFailureCode::ProcessingFailure, None)
        };
        if let Some(duplicate_id) = duplicate_id {
            if document_id.is_some_and(|id| id != duplicate_id) {
                return Err(Error::InvalidResponse(
                    "conflicting Paperless task document IDs",
                ));
            }
            document_id = Some(duplicate_id);
        }
        if value
            .get("result_data")
            .and_then(|data| data.get("duplicate_in_trash"))
            .and_then(Value::as_bool)
            == Some(true)
        {
            document_id = None;
        }
        // Legacy serializers expose related_document on FAILURE only for a
        // duplicate, including versions whose result formatter is unfamiliar.
        Some(
            if reported_failure
                && document_id.is_some()
                && value.get("related_document_ids").is_none()
                && value.get("result_data").is_none()
            {
                TaskFailureCode::Duplicate
            } else {
                code
            },
        )
    } else {
        None
    };
    Ok(Task {
        task_id: task_id.to_owned(),
        status,
        document_id,
        failure_code,
    })
}

fn classify_task_failure(value: &Value) -> (TaskFailureCode, Option<i64>) {
    let data = value.get("result_data");
    if let Some(id) = data
        .and_then(|data| data.get("duplicate_of"))
        .and_then(Value::as_i64)
        .filter(|id| *id > 0)
    {
        let in_trash = data
            .and_then(|data| data.get("duplicate_in_trash"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        return (TaskFailureCode::Duplicate, (!in_trash).then_some(id));
    }
    let result = value
        .get("result")
        .and_then(Value::as_str)
        .or_else(|| data?.get("error_message")?.as_str());
    if let Some(result) =
        result.filter(|text| text.len() <= 8192 && !text.chars().any(char::is_control))
    {
        if let Some(id) = duplicate_result_id(result) {
            return (TaskFailureCode::Duplicate, Some(id));
        }
        // These are fixed parser/consumer markers from Paperless, not arbitrary
        // document IDs or text copied into a report. Unknown results stay generic.
        let parser_error = result
            .split_once(": Error occurred while consuming document ")
            .and_then(|(filename, message)| message.strip_prefix(filename))
            .and_then(|message| message.strip_prefix(": "))
            .unwrap_or(result);
        if parser_error.starts_with("EncryptedPdfError:")
            || parser_error.starts_with("DigitalSignatureError:")
        {
            return (TaskFailureCode::ProtectedPdf, None);
        }
        if result.starts_with("Unsupported mime type ")
            || result
                .split_once(": ")
                .is_some_and(|(_, message)| message.starts_with("Unsupported mime type "))
        {
            return (TaskFailureCode::Unsupported, None);
        }
    }
    if data
        .and_then(|data| data.get("error_type"))
        .and_then(Value::as_str)
        .is_some_and(|kind| matches!(kind, "EncryptedPdfError" | "DigitalSignatureError"))
    {
        return (TaskFailureCode::ProtectedPdf, None);
    }
    (TaskFailureCode::ProcessingFailure, None)
}

/// Accept only complete known result formats. Never search arbitrary text for
/// an ID: duplicate titles themselves may contain misleading document numbers.
fn duplicate_result_id(result: &str) -> Option<i64> {
    if let Some(id) = result.strip_prefix("Not consuming: It is a duplicate of document #") {
        return strict_positive_id(id);
    }
    let (filename, message) = result.split_once(": Not consuming ")?;
    if filename.is_empty() || filename.len() > 255 {
        return None;
    }
    let duplicate = message
        .strip_prefix(filename)?
        .strip_prefix(": It is a duplicate of ")?;
    let duplicate = duplicate.strip_suffix(").")?;
    let (label, id) = duplicate.rsplit_once(" (#")?;
    if label.is_empty() || label.contains(" (#") {
        return None;
    }
    strict_positive_id(id)
}

fn strict_positive_id(value: &str) -> Option<i64> {
    (!value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| value.parse::<i64>().ok())
        .flatten()
        .filter(|id| *id > 0)
}

fn backoff(attempt: u32) -> Duration {
    Duration::from_millis(100 * 2u64.pow(attempt))
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
        None => backoff(attempt),
    };
    if delay > MAX_RETRY_DELAY {
        // Do not retry earlier than the server permits.
        return Err(Error::Http(response.status().as_u16()));
    }
    Ok(delay)
}
