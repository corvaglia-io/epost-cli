//! Durable, explicitly configured Paperless delivery of locally verified PDFs.
//!
//! An upload is recorded before its POST. If the POST outcome is uncertain, a
//! later run searches the deterministic filename and verifies original bytes;
//! it never blindly sends the PDF again. Delivery changes no ePost state.

use crate::error::{Error, Result};
use crate::paperless::{PaperlessClient, TaskStatus, normalize_url};
use crate::storage::{SavedDocument, Store};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::time::Duration;

const TARGET_NAME: &str = "paperless-target.json";
const LEDGER_NAME: &str = "paperless-delivery.json";
const MAX_ITEMS: usize = 100_000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PaperlessTarget {
    pub schema_version: u32,
    pub url: String,
    pub inbox_tag_id: i64,
    pub source_tag_id: Option<i64>,
    pub remember_token: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct DeliveryOptions {
    pub max_documents: usize,
    pub poll_attempts: u32,
    pub poll_interval_ms: u64,
}

impl Default for DeliveryOptions {
    fn default() -> Self {
        Self {
            max_documents: 1000,
            poll_attempts: 30,
            poll_interval_ms: 2000,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryPhase {
    Started,
    Uploading,
    Waiting,
    Verifying,
    Reconciling,
    Finished,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct DeliveryEvent {
    pub phase: DeliveryPhase,
    pub processed: usize,
    pub total: usize,
    pub delivered: usize,
    pub attention: usize,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryItemStatus {
    Delivered,
    AlreadyDelivered,
    Pending,
    Uncertain,
    Failed,
    Unsupported,
    DeferredLimit,
    NotAttempted,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeliveryItem {
    pub document_ref: String,
    pub status: DeliveryItemStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub document_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip)]
    authentication_failed: bool,
    #[serde(skip)]
    access_blocked: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct DeliveryReport {
    pub delivered: usize,
    pub already_delivered: usize,
    pub pending: usize,
    pub uncertain: usize,
    pub failed: usize,
    pub unsupported: usize,
    pub deferred_limit: usize,
    pub not_attempted: usize,
    pub authentication_failed: bool,
    pub access_blocked: bool,
    pub items: Vec<DeliveryItem>,
}

impl DeliveryReport {
    pub fn has_attention(&self) -> bool {
        self.authentication_failed
            || self.access_blocked
            || self.pending
                + self.uncertain
                + self.failed
                + self.unsupported
                + self.deferred_limit
                + self.not_attempted
                != 0
    }

    fn add(&mut self, item: DeliveryItem) {
        self.authentication_failed |= item.authentication_failed;
        self.access_blocked |= item.access_blocked;
        match item.status {
            DeliveryItemStatus::Delivered => self.delivered += 1,
            DeliveryItemStatus::AlreadyDelivered => self.already_delivered += 1,
            DeliveryItemStatus::Pending => self.pending += 1,
            DeliveryItemStatus::Uncertain => self.uncertain += 1,
            DeliveryItemStatus::Failed => self.failed += 1,
            DeliveryItemStatus::Unsupported => self.unsupported += 1,
            DeliveryItemStatus::DeferredLimit => self.deferred_limit += 1,
            DeliveryItemStatus::NotAttempted => self.not_attempted += 1,
        }
        self.items.push(item);
    }

    fn event(&self, phase: DeliveryPhase, total: usize) -> DeliveryEvent {
        DeliveryEvent {
            phase,
            processed: self.items.len(),
            total,
            delivered: self.delivered + self.already_delivered,
            attention: self.pending
                + self.uncertain
                + self.failed
                + self.unsupported
                + self.deferred_limit
                + self.not_attempted,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct DeliveryStatus {
    pub saved_documents: usize,
    pub verified: usize,
    pub pending: usize,
    pub uncertain: usize,
    pub task_failed: usize,
    pub unsupported: usize,
    pub not_submitted: usize,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    schema_version: u32,
    namespace_hash: String,
    url: String,
    entries: BTreeMap<String, DeliveryRecord>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeliveryRecord {
    sha256: String,
    bytes: u64,
    filename: String,
    state: DeliveryState,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum DeliveryState {
    Uploading,
    Pending { task_id: String },
    TaskFailed { task_id: String },
    Verified { document_id: i64 },
}

pub fn load_target(store: &Store) -> Result<PaperlessTarget> {
    let bytes = store.read_private_state(TARGET_NAME)?.ok_or(Error::Config(
        "Paperless is not configured for this document store",
    ))?;
    let target: PaperlessTarget = serde_json::from_slice(&bytes)
        .map_err(|_| Error::Storage("Paperless target state is corrupt"))?;
    validate_target(&target)?;
    Ok(target)
}

pub fn save_target(store: &mut Store, target: &PaperlessTarget) -> Result<()> {
    save_target_with_effect(store, target, || Ok(()), |_| Ok(()))
}

pub fn save_target_with_effect<T>(
    store: &mut Store,
    target: &PaperlessTarget,
    effect: impl FnOnce() -> Result<T>,
    rollback: impl FnOnce(T) -> Result<()>,
) -> Result<()> {
    validate_target(target)?;
    // A URL change must never send an existing ledger to a different instance.
    // Even an empty ledger stays bound; choose a separate store when migrating.
    if store.read_private_state(LEDGER_NAME)?.is_some() {
        let _ = load_ledger(store, target)?;
    }
    let raw = serde_json::to_vec_pretty(target)
        .map_err(|_| Error::Storage("Paperless state could not be serialized"))?;
    store.write_private_state_with_effect(TARGET_NAME, &raw, effect, rollback)
}

pub fn status(store: &Store, target: &PaperlessTarget) -> Result<DeliveryStatus> {
    validate_target(target)?;
    let documents = store.saved_documents()?;
    let ledger = load_ledger(store, target)?;
    let mut result = DeliveryStatus {
        saved_documents: documents.len(),
        ..Default::default()
    };
    for document in documents {
        if document.media_kind != "pdf" {
            result.unsupported += 1;
            continue;
        }
        match ledger
            .entries
            .get(&document_ref(store, &document.letter_id))
            .map(|r| &r.state)
        {
            Some(DeliveryState::Uploading) => result.uncertain += 1,
            Some(DeliveryState::Pending { .. }) => result.pending += 1,
            Some(DeliveryState::TaskFailed { .. }) => result.task_failed += 1,
            Some(DeliveryState::Verified { .. }) => result.verified += 1,
            None => result.not_submitted += 1,
        }
    }
    Ok(result)
}

pub async fn deliver(
    store: &mut Store,
    client: &PaperlessClient,
    target: &PaperlessTarget,
    options: DeliveryOptions,
    mut progress: impl FnMut(DeliveryEvent),
) -> Result<DeliveryReport> {
    validate_client(client, target)?;
    if options.max_documents == 0
        || options.max_documents > MAX_ITEMS
        || options.poll_attempts == 0
        || options.poll_attempts > 1000
        || options.poll_interval_ms > 60_000
    {
        return Err(Error::Config("Paperless delivery bounds are invalid"));
    }
    let documents = store.saved_documents()?;
    if documents.len() > MAX_ITEMS {
        return Err(Error::Storage("Paperless inventory exceeds its item limit"));
    }
    let mut ledger = load_ledger(store, target)?;
    let mut report = DeliveryReport::default();
    let mut work_count = 0;
    progress(report.event(DeliveryPhase::Started, documents.len()));
    for document in &documents {
        let reference = document_ref(store, &document.letter_id);
        if report.authentication_failed || report.access_blocked {
            report.add(item(&reference, DeliveryItemStatus::NotAttempted, None,
                Some(if report.authentication_failed {
                    "delivery stopped after Paperless authentication failed; restore access and run again"
                } else {
                    "delivery stopped after Paperless redirected access; check the installation URL and proxy sign-in, then run again"
                })));
            continue;
        }
        if document.media_kind != "pdf" {
            report.add(item(
                &reference,
                DeliveryItemStatus::Unsupported,
                None,
                Some("only validated PDF files are supported for Paperless delivery"),
            ));
            continue;
        }
        let verified = ledger
            .entries
            .get(&reference)
            .is_some_and(|record| matches!(record.state, DeliveryState::Verified { .. }));
        if !verified {
            if work_count >= options.max_documents {
                report.add(item(
                    &reference,
                    DeliveryItemStatus::DeferredLimit,
                    None,
                    Some("delivery limit reached; run again to continue"),
                ));
                continue;
            }
            work_count += 1;
        }
        // Verify local bytes on every run before any remote operation, including
        // already-confirmed deliveries and reconciliation after an interruption.
        let bytes = store.read_saved(&document.letter_id)?;
        let record = match ledger.entries.get(&reference).cloned() {
            Some(record) => record,
            None => {
                let record = DeliveryRecord {
                    sha256: document.sha256.clone(),
                    bytes: document.bytes,
                    filename: upload_filename(store, document),
                    state: DeliveryState::Uploading,
                };
                let tags: Vec<i64> = std::iter::once(target.inbox_tag_id)
                    .chain(target.source_tag_id)
                    .collect();
                if let Err(error) = client.validate_upload(&record.filename, &bytes, &tags) {
                    report.add(item_error(
                        &reference,
                        DeliveryItemStatus::Failed,
                        None,
                        &error,
                    ));
                    continue;
                }
                ledger.entries.insert(reference.clone(), record.clone());
                persist(store, LEDGER_NAME, &ledger)?;
                progress(report.event(DeliveryPhase::Uploading, documents.len()));
                match client.upload_pdf(&record.filename, bytes, &tags).await {
                    Ok(task_id) => {
                        if !valid_task_id(&task_id) {
                            report.add(item(&reference, DeliveryItemStatus::Uncertain, None,
                                Some("upload outcome is uncertain; invalid task identifier; reconcile before retrying")));
                            continue;
                        }
                        let pending = DeliveryRecord {
                            state: DeliveryState::Pending { task_id },
                            ..record
                        };
                        ledger.entries.insert(reference.clone(), pending.clone());
                        persist(store, LEDGER_NAME, &ledger)?;
                        pending
                    }
                    Err(error) => {
                        report.add(item_error(
                            &reference,
                            DeliveryItemStatus::Uncertain,
                            None,
                            &error,
                        ));
                        continue;
                    }
                }
            }
        };
        let result = match &record.state {
            DeliveryState::Verified { document_id } => {
                progress(report.event(DeliveryPhase::Verifying, documents.len()));
                match verify_original(client, *document_id, document).await {
                    Ok(()) => item(
                        &reference,
                        DeliveryItemStatus::AlreadyDelivered,
                        Some(*document_id),
                        None,
                    ),
                    Err(error) => item_error(
                        &reference,
                        DeliveryItemStatus::Failed,
                        Some(*document_id),
                        &error,
                    ),
                }
            }
            DeliveryState::Uploading | DeliveryState::TaskFailed { .. } => {
                progress(report.event(DeliveryPhase::Reconciling, documents.len()));
                reconcile_record(
                    store,
                    client,
                    target,
                    &mut ledger,
                    &reference,
                    document,
                    &record,
                )
                .await?
            }
            DeliveryState::Pending { task_id } => {
                let mut result = item(
                    &reference,
                    DeliveryItemStatus::Pending,
                    None,
                    Some("Paperless is still processing this upload; the next run will resume it"),
                );
                let mut task_missing = false;
                for attempt in 0..options.poll_attempts {
                    if attempt != 0 && options.poll_interval_ms != 0 {
                        tokio::time::sleep(Duration::from_millis(options.poll_interval_ms)).await;
                    }
                    progress(report.event(DeliveryPhase::Waiting, documents.len()));
                    let task = match client.task(task_id).await {
                        Ok(Some(task)) if task.task_id == *task_id => task,
                        Ok(None) => {
                            task_missing = true;
                            continue;
                        }
                        Ok(Some(_)) => {
                            result = item(
                                &reference,
                                DeliveryItemStatus::Failed,
                                None,
                                Some("Paperless returned a different task identifier"),
                            );
                            break;
                        }
                        Err(error) => {
                            result =
                                item_error(&reference, DeliveryItemStatus::Pending, None, &error);
                            break;
                        }
                    };
                    task_missing = false;
                    match task.status {
                        TaskStatus::Pending => continue,
                        TaskStatus::Failure => {
                            let failed = DeliveryRecord {
                                state: DeliveryState::TaskFailed {
                                    task_id: task_id.clone(),
                                },
                                ..record.clone()
                            };
                            ledger.entries.insert(reference.clone(), failed.clone());
                            persist(store, LEDGER_NAME, &ledger)?;
                            progress(report.event(DeliveryPhase::Reconciling, documents.len()));
                            result = reconcile_record(
                                store,
                                client,
                                target,
                                &mut ledger,
                                &reference,
                                document,
                                &failed,
                            )
                            .await?;
                            break;
                        }
                        TaskStatus::Success => {
                            let Some(document_id) = task.document_id.filter(|id| *id > 0) else {
                                progress(report.event(DeliveryPhase::Reconciling, documents.len()));
                                result = reconcile_record(
                                    store,
                                    client,
                                    target,
                                    &mut ledger,
                                    &reference,
                                    document,
                                    &record,
                                )
                                .await?;
                                break;
                            };
                            progress(report.event(DeliveryPhase::Verifying, documents.len()));
                            match verify_new_document(client, target, document_id, document).await {
                                Ok(()) => {
                                    mark_verified(store, &mut ledger, &reference, document_id)?;
                                    result = item(
                                        &reference,
                                        DeliveryItemStatus::Delivered,
                                        Some(document_id),
                                        None,
                                    );
                                }
                                Err(error) => {
                                    result = item_error(
                                        &reference,
                                        DeliveryItemStatus::Failed,
                                        Some(document_id),
                                        &error,
                                    )
                                }
                            }
                            break;
                        }
                    }
                }
                if task_missing && !result.authentication_failed && !result.access_blocked {
                    progress(report.event(DeliveryPhase::Reconciling, documents.len()));
                    result = reconcile_record(
                        store,
                        client,
                        target,
                        &mut ledger,
                        &reference,
                        document,
                        &record,
                    )
                    .await?;
                }
                result
            }
        };
        report.add(result);
    }
    progress(report.event(DeliveryPhase::Finished, documents.len()));
    Ok(report)
}

/// Explicit manual adoption still requires byte-for-byte original verification
/// and the configured review tags. This cannot archive or mark ePost mail read.
pub async fn reconcile_document(
    store: &mut Store,
    client: &PaperlessClient,
    target: &PaperlessTarget,
    letter_id: &str,
    document_id: i64,
) -> Result<DeliveryItem> {
    validate_client(client, target)?;
    if document_id <= 0 {
        return Err(Error::Config(
            "Paperless document identifier must be positive",
        ));
    }
    let document = store
        .saved_documents()?
        .into_iter()
        .find(|d| d.letter_id == letter_id)
        .ok_or(Error::Storage("saved document is missing"))?;
    if document.media_kind != "pdf" {
        return Err(Error::Config("only validated PDFs can be reconciled"));
    }
    let _ = store.read_saved(letter_id)?;
    let reference = document_ref(store, letter_id);
    let mut ledger = load_ledger(store, target)?;
    if let Some(DeliveryRecord {
        state: DeliveryState::Verified {
            document_id: saved_id,
        },
        ..
    }) = ledger.entries.get(&reference)
    {
        if *saved_id != document_id {
            return Err(Error::Config(
                "delivery already refers to a different verified document",
            ));
        }
        verify_original(client, document_id, &document).await?;
        return Ok(item(
            &reference,
            DeliveryItemStatus::AlreadyDelivered,
            Some(document_id),
            None,
        ));
    }
    verify_new_document(client, target, document_id, &document).await?;
    ledger.entries.insert(
        reference.clone(),
        DeliveryRecord {
            sha256: document.sha256.clone(),
            bytes: document.bytes,
            filename: upload_filename(store, &document),
            state: DeliveryState::Verified { document_id },
        },
    );
    persist(store, LEDGER_NAME, &ledger)?;
    Ok(item(
        &reference,
        DeliveryItemStatus::Delivered,
        Some(document_id),
        None,
    ))
}

async fn reconcile_record(
    store: &mut Store,
    client: &PaperlessClient,
    target: &PaperlessTarget,
    ledger: &mut Ledger,
    reference: &str,
    document: &SavedDocument,
    record: &DeliveryRecord,
) -> Result<DeliveryItem> {
    let failure_status = if matches!(record.state, DeliveryState::TaskFailed { .. }) {
        DeliveryItemStatus::Failed
    } else {
        DeliveryItemStatus::Uncertain
    };
    let candidates = match client.find_documents(&record.filename).await {
        Ok(candidates) => candidates,
        Err(error) => return Ok(item_error(reference, failure_status, None, &error)),
    };
    if candidates.len() > 1000 {
        return Ok(item(
            reference,
            failure_status,
            None,
            Some("too many reconciliation candidates; choose a document explicitly"),
        ));
    }
    let mut matching = Vec::new();
    for id in candidates {
        if id <= 0 {
            return Ok(item(
                reference,
                failure_status,
                None,
                Some("Paperless returned an invalid document identifier"),
            ));
        }
        if let Err(error) = verify_original(client, id, document).await {
            // An inaccessible original is not proof of a mismatching document.
            if matches!(
                error,
                Error::InvalidResponse("Paperless original does not match the local PDF")
            ) {
                continue;
            }
            return Ok(item_error(reference, failure_status, Some(id), &error));
        }
        matching.push(id);
    }
    matching.sort_unstable();
    matching.dedup();
    match matching.as_slice() {
        [document_id] => {
            // The original was already checked above; avoid downloading it a
            // second time merely to check the review queue metadata.
            if let Err(error) = verify_review_tags(client, target, *document_id).await {
                return Ok(item_error(
                    reference,
                    failure_status,
                    Some(*document_id),
                    &error,
                ));
            }
            mark_verified(store, ledger, reference, *document_id)?;
            Ok(item(
                reference,
                DeliveryItemStatus::Delivered,
                Some(*document_id),
                None,
            ))
        }
        [] => Ok(item(
            reference,
            failure_status,
            None,
            Some(
                "upload outcome cannot be confirmed; no automatic resubmission; reconcile a matching Paperless document explicitly",
            ),
        )),
        _ => Ok(item(
            reference,
            failure_status,
            None,
            Some(
                "multiple Paperless originals match this upload; choose a document explicitly for reconciliation",
            ),
        )),
    }
}

async fn verify_original(client: &PaperlessClient, id: i64, local: &SavedDocument) -> Result<()> {
    let bytes = client.original(id).await?;
    if bytes.len() as u64 != local.bytes || digest(&bytes) != local.sha256 {
        return Err(Error::InvalidResponse(
            "Paperless original does not match the local PDF",
        ));
    }
    Ok(())
}

async fn verify_new_document(
    client: &PaperlessClient,
    target: &PaperlessTarget,
    id: i64,
    local: &SavedDocument,
) -> Result<()> {
    verify_original(client, id, local).await?;
    verify_review_tags(client, target, id).await
}

async fn verify_review_tags(
    client: &PaperlessClient,
    target: &PaperlessTarget,
    id: i64,
) -> Result<()> {
    if !client.tag(target.inbox_tag_id).await?.is_inbox_tag {
        return Err(Error::InvalidResponse(
            "configured Paperless review tag is no longer an inbox tag",
        ));
    }
    let remote = client.document(id).await?;
    if remote.id != id
        || !remote.tags.contains(&target.inbox_tag_id)
        || target
            .source_tag_id
            .is_some_and(|tag| !remote.tags.contains(&tag))
    {
        return Err(Error::InvalidResponse(
            "Paperless document is missing its configured review tags",
        ));
    }
    Ok(())
}

fn mark_verified(
    store: &mut Store,
    ledger: &mut Ledger,
    reference: &str,
    document_id: i64,
) -> Result<()> {
    ledger
        .entries
        .get_mut(reference)
        .ok_or(Error::Storage("Paperless delivery record is missing"))?
        .state = DeliveryState::Verified { document_id };
    persist(store, LEDGER_NAME, ledger)
}

fn load_ledger(store: &Store, target: &PaperlessTarget) -> Result<Ledger> {
    let ledger = match store.read_private_state(LEDGER_NAME)? {
        Some(raw) => serde_json::from_slice::<Ledger>(&raw)
            .map_err(|_| Error::Storage("Paperless delivery ledger is corrupt"))?,
        None => Ledger {
            schema_version: 1,
            namespace_hash: store.identity_hash().to_owned(),
            url: target.url.clone(),
            entries: BTreeMap::new(),
        },
    };
    if ledger.schema_version != 1
        || ledger.namespace_hash != store.identity_hash()
        || ledger.url != target.url
        || ledger.entries.len() > MAX_ITEMS
    {
        return Err(Error::Storage(
            "Paperless ledger namespace, instance or version is invalid",
        ));
    }
    let documents: BTreeMap<String, SavedDocument> = store
        .saved_documents()?
        .into_iter()
        .map(|d| (document_ref(store, &d.letter_id), d))
        .collect();
    for (reference, record) in &ledger.entries {
        let local = documents.get(reference).ok_or(Error::Storage(
            "Paperless ledger refers to an unknown local document",
        ))?;
        if local.media_kind != "pdf"
            || local.sha256 != record.sha256
            || local.bytes != record.bytes
            || record.filename != upload_filename(store, local)
        {
            return Err(Error::Storage(
                "Paperless delivery record conflicts with its local document",
            ));
        }
        match &record.state {
            DeliveryState::Pending { task_id } | DeliveryState::TaskFailed { task_id }
                if !valid_task_id(task_id) =>
            {
                return Err(Error::Storage(
                    "Paperless delivery task identifier is invalid",
                ));
            }
            DeliveryState::Verified { document_id } if *document_id <= 0 => {
                return Err(Error::Storage(
                    "Paperless delivery document identifier is invalid",
                ));
            }
            _ => (),
        }
    }
    Ok(ledger)
}

fn validate_target(target: &PaperlessTarget) -> Result<()> {
    if target.schema_version != 1
        || target.inbox_tag_id <= 0
        || target.source_tag_id.is_some_and(|id| id <= 0)
    {
        return Err(Error::Config(
            "Paperless target version or tag identifiers are invalid",
        ));
    }
    if normalize_url(&target.url, true)? != target.url {
        return Err(Error::Config("Paperless target URL must be canonical"));
    }
    Ok(())
}

fn validate_client(client: &PaperlessClient, target: &PaperlessTarget) -> Result<()> {
    validate_target(target)?;
    if client.canonical_url() != target.url {
        return Err(Error::Config(
            "Paperless client does not match the configured instance",
        ));
    }
    Ok(())
}

fn persist<T: Serialize>(store: &mut Store, name: &str, value: &T) -> Result<()> {
    let raw = serde_json::to_vec_pretty(value)
        .map_err(|_| Error::Storage("Paperless state could not be serialized"))?;
    store.write_private_state(name, &raw)
}

fn document_ref(store: &Store, letter_id: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(b"epost-paperless-document-v1\0");
    hash.update(store.identity_hash().as_bytes());
    hash.update(b"\0");
    hash.update(letter_id.as_bytes());
    hex::encode(hash.finalize())
}

fn upload_filename(store: &Store, document: &SavedDocument) -> String {
    format!(
        "epost-{}-{}-{}.pdf",
        store.identity_hash(),
        document_ref(store, &document.letter_id),
        document.sha256
    )
}

fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn valid_task_id(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(index, byte)| {
            if [8, 13, 18, 23].contains(&index) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

fn item(
    reference: &str,
    status: DeliveryItemStatus,
    document_id: Option<i64>,
    error: Option<&str>,
) -> DeliveryItem {
    DeliveryItem {
        document_ref: reference.to_owned(),
        status,
        document_id,
        error: error.map(str::to_owned),
        authentication_failed: false,
        access_blocked: false,
    }
}

fn item_error(
    reference: &str,
    status: DeliveryItemStatus,
    document_id: Option<i64>,
    error: &Error,
) -> DeliveryItem {
    DeliveryItem {
        document_ref: reference.to_owned(),
        status,
        document_id,
        error: Some(error.to_string()),
        authentication_failed: matches!(error, Error::Auth | Error::Http(401 | 403)),
        access_blocked: matches!(error, Error::PaperlessRedirect { .. }),
    }
}
