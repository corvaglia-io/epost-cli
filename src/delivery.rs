//! Durable, explicitly configured Paperless delivery of locally verified PDFs.
//!
//! An upload is recorded before its POST. If the POST outcome is uncertain, a
//! later run searches the deterministic filename and verifies original bytes;
//! it never blindly sends the PDF again. Delivery changes no ePost state.

use crate::destinations::{
    CredentialBinding, Destination, DestinationKind, DocumentTypePolicy, MetadataValue,
    PaperlessDestination, SourceField, destination_identity, ledger_name, validate_destination,
};
use crate::error::{Error, Result};
use crate::paperless::{
    PaperlessClient, TaskFailureCode, TaskStatus, UploadMetadata, normalize_url,
};
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

#[derive(Debug, Clone, Copy, Default)]
pub struct NamedDeliveryOptions {
    pub delivery: DeliveryOptions,
    pub audit: bool,
}

#[derive(Default)]
struct DeliveryContext {
    ledger_name: String,
    destination_identity: Option<String>,
    config: Option<PaperlessDestination>,
    cached_verified: bool,
    verify_only: bool,
}

impl DeliveryContext {
    fn legacy() -> Self {
        Self {
            ledger_name: LEDGER_NAME.into(),
            ..Default::default()
        }
    }
    fn named(store: &Store, destination: &Destination, audit: bool) -> Result<Self> {
        validate_destination(destination)?;
        let DestinationKind::Paperless(config) = &destination.kind else {
            return Err(Error::Config("destination is not Paperless"));
        };
        Ok(Self {
            ledger_name: ledger_name(destination),
            destination_identity: (destination.credential_binding == CredentialBinding::Named)
                .then(|| destination_identity(store, destination)),
            config: Some(config.clone()),
            cached_verified: !audit,
            verify_only: false,
        })
    }
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
    PreviouslyConfirmed,
    Pending,
    Uncertain,
    Failed,
    Unsupported,
    DeferredLimit,
    NotAttempted,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryReasonCode {
    Duplicate,
    ProtectedPdf,
    Unsupported,
    ProcessingFailure,
    UploadOutcomeUnknown,
    ReconciliationAmbiguous,
    Pending,
    UnsupportedContent,
    VerificationFailed,
    AuthenticationFailed,
    AccessBlocked,
    DeferredLimit,
    NotAttempted,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryRecoveryAction {
    Resume,
    ReconcileExplicitly,
    InspectPaperless,
    ReviewSource,
    RestoreAccess,
    CheckDestination,
    RetryVerification,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeliveryItem {
    pub document_ref: String,
    pub status: DeliveryItemStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub document_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<DeliveryReasonCode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovery_action: Option<DeliveryRecoveryAction>,
    #[serde(skip)]
    authentication_failed: bool,
    #[serde(skip)]
    access_blocked: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct DeliveryReport {
    pub delivered: usize,
    pub already_delivered: usize,
    pub previously_confirmed: usize,
    pub pending: usize,
    pub uncertain: usize,
    pub failed: usize,
    pub unsupported: usize,
    pub deferred_limit: usize,
    pub not_attempted: usize,
    pub authentication_failed: bool,
    pub access_blocked: bool,
    pub cancelled: bool,
    pub items: Vec<DeliveryItem>,
}

impl DeliveryReport {
    pub fn has_attention(&self) -> bool {
        self.cancelled
            || self.authentication_failed
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
            DeliveryItemStatus::PreviouslyConfirmed => self.previously_confirmed += 1,
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
            delivered: self.delivered + self.already_delivered + self.previously_confirmed,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    destination_identity: Option<String>,
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
    Pending {
        task_id: String,
    },
    TaskFailed {
        task_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        failure_code: Option<TaskFailureCode>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        related_document_id: Option<i64>,
    },
    Verified {
        document_id: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provenance: Option<VerificationProvenance>,
    },
}

#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum VerificationProvenance {
    ExistingDuplicate,
    ExistingDocument,
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
    progress: impl FnMut(DeliveryEvent),
) -> Result<DeliveryReport> {
    deliver_inner(
        store,
        client,
        target,
        options,
        &DeliveryContext::legacy(),
        progress,
    )
    .await
}

pub async fn deliver_paperless(
    store: &mut Store,
    client: &PaperlessClient,
    destination: &Destination,
    options: NamedDeliveryOptions,
    progress: impl FnMut(DeliveryEvent),
) -> Result<DeliveryReport> {
    let context = DeliveryContext::named(store, destination, options.audit)?;
    let target = &context
        .config
        .as_ref()
        .expect("named context has configuration")
        .target;
    deliver_inner(store, client, target, options.delivery, &context, progress).await
}

/// Audit retained records without submitting any unrecorded local PDF.
pub async fn verify_paperless(
    store: &mut Store,
    client: &PaperlessClient,
    destination: &Destination,
    options: DeliveryOptions,
    progress: impl FnMut(DeliveryEvent),
) -> Result<DeliveryReport> {
    let mut context = DeliveryContext::named(store, destination, true)?;
    context.verify_only = true;
    let target = &context
        .config
        .as_ref()
        .expect("named context has configuration")
        .target;
    deliver_inner(store, client, target, options, &context, progress).await
}

async fn deliver_inner(
    store: &mut Store,
    client: &PaperlessClient,
    target: &PaperlessTarget,
    options: DeliveryOptions,
    context: &DeliveryContext,
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
    let mut ledger = load_ledger_for(store, target, context)?;
    let mut report = DeliveryReport::default();
    let mut work_count = 0;
    progress(report.event(DeliveryPhase::Started, documents.len()));
    for document in &documents {
        let reference = document_ref(store, &document.letter_id);
        if context.config.is_some() && crate::progress::stop_requested() {
            report.cancelled = true;
            report.add(item(
                &reference,
                DeliveryItemStatus::NotAttempted,
                None,
                Some("delivery stopped at a safe boundary; run again to continue"),
            ));
            continue;
        }
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
        if context.verify_only && !ledger.entries.contains_key(&reference) {
            report.add(item(
                &reference,
                DeliveryItemStatus::NotAttempted,
                None,
                Some("this PDF has not been submitted to the destination; run check to deliver it"),
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
        if context.cached_verified
            && let Some(DeliveryRecord {
                state: DeliveryState::Verified { document_id, .. },
                ..
            }) = ledger.entries.get(&reference)
        {
            report.add(item(
                &reference,
                DeliveryItemStatus::PreviouslyConfirmed,
                Some(*document_id),
                None,
            ));
            continue;
        }
        let record = match ledger.entries.get(&reference).cloned() {
            Some(record) => record,
            None => {
                let record = DeliveryRecord {
                    sha256: document.sha256.clone(),
                    bytes: document.bytes,
                    filename: upload_filename_for(store, document, context),
                    state: DeliveryState::Uploading,
                };
                let metadata = upload_metadata(store, document, target, context)?;
                if let Err(error) =
                    client.validate_upload_metadata(&record.filename, &bytes, &metadata)
                {
                    report.add(item_error(
                        &reference,
                        DeliveryItemStatus::Failed,
                        None,
                        &error,
                    ));
                    continue;
                }
                if let Err(error) = validate_configured_metadata(client, context).await {
                    report.add(item_error(
                        &reference,
                        DeliveryItemStatus::Failed,
                        None,
                        &error,
                    ));
                    continue;
                }
                ledger.entries.insert(reference.clone(), record.clone());
                persist(store, &context.ledger_name, &ledger)?;
                progress(report.event(DeliveryPhase::Uploading, documents.len()));
                match client
                    .upload_pdf_with_metadata(&record.filename, bytes, &metadata)
                    .await
                {
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
                        persist(store, &context.ledger_name, &ledger)?;
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
            DeliveryState::Verified { document_id, .. } => {
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
            DeliveryState::Uploading => {
                progress(report.event(DeliveryPhase::Reconciling, documents.len()));
                reconcile_record(
                    store,
                    client,
                    target,
                    &mut ledger,
                    document,
                    &record,
                    context,
                )
                .await?
            }
            DeliveryState::TaskFailed {
                task_id,
                failure_code,
                related_document_id,
            } => {
                // Older journals retained only the task ID. A GET can recover
                // its redacted category/reference; a vanished task never erases
                // the failure already observed and never authorizes a repost.
                let mut failed = record.clone();
                match client.task(task_id).await {
                    Ok(Some(task)) if task.status == TaskStatus::Failure => {
                        if related_document_id
                            .zip(task.document_id)
                            .is_some_and(|(known, current)| known != current)
                        {
                            report.add(item(
                                &reference,
                                DeliveryItemStatus::Failed,
                                None,
                                Some("Paperless task reference changed; choose a document explicitly for reconciliation"),
                            ));
                            continue;
                        }
                        let refreshed_code = match (failure_code, task.failure_code) {
                            // Sparse/expired result details are not evidence
                            // against the category retained from the same task.
                            (Some(known), Some(TaskFailureCode::ProcessingFailure)) => Some(*known),
                            (_, current) => current.or(*failure_code),
                        };
                        failed.state = DeliveryState::TaskFailed {
                            task_id: task_id.clone(),
                            failure_code: refreshed_code,
                            related_document_id: task.document_id.or(*related_document_id),
                        };
                        ledger.entries.insert(reference.clone(), failed.clone());
                        persist(store, &context.ledger_name, &ledger)?;
                    }
                    Ok(_) | Err(Error::Http(404)) => (),
                    Err(error) => {
                        report.add(item_error(
                            &reference,
                            DeliveryItemStatus::Failed,
                            None,
                            &error,
                        ));
                        continue;
                    }
                }
                progress(report.event(DeliveryPhase::Reconciling, documents.len()));
                reconcile_record(
                    store,
                    client,
                    target,
                    &mut ledger,
                    document,
                    &failed,
                    context,
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
                    if context.config.is_some() && crate::progress::stop_requested() {
                        report.cancelled = true;
                        break;
                    }
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
                            result = item_error(
                                &reference,
                                if matches!(error, Error::InvalidResponse(_)) {
                                    DeliveryItemStatus::Failed
                                } else {
                                    DeliveryItemStatus::Pending
                                },
                                None,
                                &error,
                            );
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
                                    failure_code: task.failure_code,
                                    related_document_id: task.document_id,
                                },
                                ..record.clone()
                            };
                            ledger.entries.insert(reference.clone(), failed.clone());
                            persist(store, &context.ledger_name, &ledger)?;
                            progress(report.event(DeliveryPhase::Reconciling, documents.len()));
                            result = reconcile_record(
                                store,
                                client,
                                target,
                                &mut ledger,
                                document,
                                &failed,
                                context,
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
                                    document,
                                    &record,
                                    context,
                                )
                                .await?;
                                break;
                            };
                            progress(report.event(DeliveryPhase::Verifying, documents.len()));
                            match verify_new_document_for(
                                client,
                                target,
                                document_id,
                                document,
                                context,
                            )
                            .await
                            {
                                Ok(()) => {
                                    mark_verified_for(
                                        store,
                                        &mut ledger,
                                        &reference,
                                        document_id,
                                        context,
                                    )?;
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
                        document,
                        &record,
                        context,
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
    reconcile_document_for(
        store,
        client,
        target,
        letter_id,
        document_id,
        &DeliveryContext::legacy(),
        false,
    )
    .await
}

/// Adopt an explicitly selected existing document after verifying its original
/// bytes. Preserve its metadata/review state and bind the proof to this named
/// destination. Automatic reconciliation still enforces new-upload review tags.
pub async fn reconcile_paperless(
    store: &mut Store,
    client: &PaperlessClient,
    destination: &Destination,
    letter_id: &str,
    document_id: i64,
) -> Result<DeliveryItem> {
    let context = DeliveryContext::named(store, destination, true)?;
    let target = &context.config.as_ref().expect("named configuration").target;
    reconcile_document_for(
        store,
        client,
        target,
        letter_id,
        document_id,
        &context,
        true,
    )
    .await
}

async fn reconcile_document_for(
    store: &mut Store,
    client: &PaperlessClient,
    target: &PaperlessTarget,
    letter_id: &str,
    document_id: i64,
    context: &DeliveryContext,
    explicit_existing: bool,
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
    let mut ledger = load_ledger_for(store, target, context)?;
    if let Some(DeliveryRecord {
        state: DeliveryState::Verified {
            document_id: saved_id,
            ..
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
    let duplicate = ledger.entries.get(&reference).is_some_and(|record| {
        matches!(
            record.state,
            DeliveryState::TaskFailed {
                failure_code: Some(TaskFailureCode::Duplicate),
                ..
            }
        )
    });
    if duplicate || explicit_existing {
        verify_original(client, document_id, &document).await?;
    } else {
        verify_new_document_for(client, target, document_id, &document, context).await?;
    }
    ledger.entries.insert(
        reference.clone(),
        DeliveryRecord {
            sha256: document.sha256.clone(),
            bytes: document.bytes,
            filename: upload_filename_for(store, &document, context),
            state: DeliveryState::Verified {
                document_id,
                provenance: if duplicate {
                    Some(VerificationProvenance::ExistingDuplicate)
                } else if explicit_existing {
                    Some(VerificationProvenance::ExistingDocument)
                } else {
                    None
                },
            },
        },
    );
    persist(store, &context.ledger_name, &ledger)?;
    let mut result = item(
        &reference,
        if duplicate || explicit_existing {
            DeliveryItemStatus::AlreadyDelivered
        } else {
            DeliveryItemStatus::Delivered
        },
        Some(document_id),
        None,
    );
    if duplicate {
        result.reason_code = Some(DeliveryReasonCode::Duplicate);
    }
    Ok(result)
}

async fn reconcile_record(
    store: &mut Store,
    client: &PaperlessClient,
    target: &PaperlessTarget,
    ledger: &mut Ledger,
    document: &SavedDocument,
    record: &DeliveryRecord,
    context: &DeliveryContext,
) -> Result<DeliveryItem> {
    let reference = document_ref(store, &document.letter_id);
    let reference = reference.as_str();
    let failure_code = match &record.state {
        DeliveryState::TaskFailed { failure_code, .. } => {
            Some(failure_code.unwrap_or(TaskFailureCode::ProcessingFailure))
        }
        _ => None,
    };
    let duplicate = failure_code == Some(TaskFailureCode::Duplicate);
    let failure_status = if matches!(record.state, DeliveryState::TaskFailed { .. }) {
        DeliveryItemStatus::Failed
    } else {
        DeliveryItemStatus::Uncertain
    };
    if let DeliveryState::TaskFailed {
        related_document_id: Some(id),
        ..
    } = record.state
    {
        let verified = if duplicate {
            // This PDF was already present before the attempted ingestion.
            // Its review state and metadata belong to that existing document.
            verify_original(client, id, document).await
        } else {
            verify_new_document_for(client, target, id, document, context).await
        };
        return Ok(match verified {
            Ok(()) => {
                mark_verified_with_provenance(
                    store,
                    ledger,
                    reference,
                    id,
                    duplicate.then_some(VerificationProvenance::ExistingDuplicate),
                    context,
                )?;
                confirmed_reconciliation_item(reference, id, duplicate)
            }
            Err(error) => {
                let mut result = item_error(reference, failure_status, Some(id), &error);
                if duplicate && !result.authentication_failed && !result.access_blocked {
                    // An unavailable or mismatching original does not erase
                    // Paperless's confirmed duplicate rejection. Keep explicit
                    // reconciliation guidance without adopting unverified bytes.
                    result.reason_code = Some(DeliveryReasonCode::Duplicate);
                    result.recovery_action = Some(DeliveryRecoveryAction::ReconcileExplicitly);
                }
                result
            }
        });
    }
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
            if !duplicate
                && let Err(error) =
                    verify_review_tags_for(client, target, *document_id, context).await
            {
                return Ok(item_error(
                    reference,
                    failure_status,
                    Some(*document_id),
                    &error,
                ));
            }
            mark_verified_with_provenance(
                store,
                ledger,
                reference,
                *document_id,
                duplicate.then_some(VerificationProvenance::ExistingDuplicate),
                context,
            )?;
            Ok(confirmed_reconciliation_item(
                reference,
                *document_id,
                duplicate,
            ))
        }
        [] if failure_code.is_some() => Ok(task_failure_item(
            reference,
            failure_code.expect("known failure"),
        )),
        [] => Ok(item(
            reference,
            failure_status,
            None,
            Some(
                "upload outcome cannot be confirmed; no automatic resubmission; reconcile a matching Paperless document explicitly",
            ),
        )),
        _ => {
            let mut result = item(
                reference,
                failure_status,
                None,
                Some(
                    "multiple Paperless originals match this upload; choose a document explicitly for reconciliation",
                ),
            );
            result.reason_code = Some(DeliveryReasonCode::ReconciliationAmbiguous);
            result.recovery_action = Some(DeliveryRecoveryAction::ReconcileExplicitly);
            Ok(result)
        }
    }
}

fn confirmed_reconciliation_item(reference: &str, id: i64, duplicate: bool) -> DeliveryItem {
    let mut result = item(
        reference,
        if duplicate {
            DeliveryItemStatus::AlreadyDelivered
        } else {
            DeliveryItemStatus::Delivered
        },
        Some(id),
        None,
    );
    if duplicate {
        result.reason_code = Some(DeliveryReasonCode::Duplicate);
    }
    result
}

fn task_failure_item(reference: &str, code: TaskFailureCode) -> DeliveryItem {
    let (reason, message, action) = match code {
        TaskFailureCode::Duplicate => (
            DeliveryReasonCode::Duplicate,
            "Paperless rejected a duplicate; its existing original has not been verified; reconcile a matching document explicitly; no automatic resubmission",
            DeliveryRecoveryAction::ReconcileExplicitly,
        ),
        TaskFailureCode::ProtectedPdf => (
            DeliveryReasonCode::ProtectedPdf,
            "Paperless reported a protected PDF processing failure; inspect the task in Paperless; a matching PDF may already be present; no automatic resubmission",
            DeliveryRecoveryAction::InspectPaperless,
        ),
        TaskFailureCode::Unsupported => (
            DeliveryReasonCode::Unsupported,
            "Paperless reported an unsupported file type; inspect the task in Paperless; a matching PDF may already be present; no automatic resubmission",
            DeliveryRecoveryAction::InspectPaperless,
        ),
        TaskFailureCode::ProcessingFailure => (
            DeliveryReasonCode::ProcessingFailure,
            "Paperless reported an ingestion task failure; inspect the task in Paperless; a matching PDF may already be present; no automatic resubmission",
            DeliveryRecoveryAction::InspectPaperless,
        ),
    };
    let mut result = item(reference, DeliveryItemStatus::Failed, None, Some(message));
    result.reason_code = Some(reason);
    result.recovery_action = Some(action);
    result
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

async fn verify_new_document_for(
    client: &PaperlessClient,
    target: &PaperlessTarget,
    id: i64,
    local: &SavedDocument,
    context: &DeliveryContext,
) -> Result<()> {
    verify_original(client, id, local).await?;
    verify_review_tags_for(client, target, id, context).await
}

async fn verify_review_tags_for(
    client: &PaperlessClient,
    target: &PaperlessTarget,
    id: i64,
    context: &DeliveryContext,
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
        || context.config.as_ref().is_some_and(|config| {
            config
                .extra_tag_ids
                .iter()
                .any(|tag| !remote.tags.contains(tag))
        })
    {
        return Err(Error::InvalidResponse(
            "Paperless document is missing its configured review tags",
        ));
    }
    Ok(())
}

fn mark_verified_for(
    store: &mut Store,
    ledger: &mut Ledger,
    reference: &str,
    document_id: i64,
    context: &DeliveryContext,
) -> Result<()> {
    mark_verified_with_provenance(store, ledger, reference, document_id, None, context)
}

fn mark_verified_with_provenance(
    store: &mut Store,
    ledger: &mut Ledger,
    reference: &str,
    document_id: i64,
    provenance: Option<VerificationProvenance>,
    context: &DeliveryContext,
) -> Result<()> {
    ledger
        .entries
        .get_mut(reference)
        .ok_or(Error::Storage("Paperless delivery record is missing"))?
        .state = DeliveryState::Verified {
        document_id,
        provenance,
    };
    persist(store, &context.ledger_name, ledger)
}

fn load_ledger(store: &Store, target: &PaperlessTarget) -> Result<Ledger> {
    load_ledger_for(store, target, &DeliveryContext::legacy())
}

fn load_ledger_for(
    store: &Store,
    target: &PaperlessTarget,
    context: &DeliveryContext,
) -> Result<Ledger> {
    let ledger = match store.read_private_state(&context.ledger_name)? {
        Some(raw) => serde_json::from_slice::<Ledger>(&raw)
            .map_err(|_| Error::Storage("Paperless delivery ledger is corrupt"))?,
        None => Ledger {
            schema_version: 1,
            namespace_hash: store.identity_hash().to_owned(),
            url: target.url.clone(),
            destination_identity: context.destination_identity.clone(),
            entries: BTreeMap::new(),
        },
    };
    if ledger.schema_version != 1
        || ledger.namespace_hash != store.identity_hash()
        || ledger.url != target.url
        || ledger.destination_identity != context.destination_identity
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
            || record.filename != upload_filename_for(store, local, context)
        {
            return Err(Error::Storage(
                "Paperless delivery record conflicts with its local document",
            ));
        }
        match &record.state {
            DeliveryState::Pending { task_id } | DeliveryState::TaskFailed { task_id, .. }
                if !valid_task_id(task_id) =>
            {
                return Err(Error::Storage(
                    "Paperless delivery task identifier is invalid",
                ));
            }
            DeliveryState::Verified { document_id, .. } if *document_id <= 0 => {
                return Err(Error::Storage(
                    "Paperless delivery document identifier is invalid",
                ));
            }
            DeliveryState::TaskFailed {
                related_document_id: Some(document_id),
                ..
            } if *document_id <= 0 => {
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

fn upload_filename_for(
    store: &Store,
    document: &SavedDocument,
    context: &DeliveryContext,
) -> String {
    if let Some(binding) = &context.destination_identity {
        // Include destination identity without exceeding 255 filename bytes.
        return format!(
            "epost-{}-{}-{}.pdf",
            binding,
            document_ref(store, &document.letter_id),
            document.sha256
        );
    }
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

fn upload_metadata(
    store: &Store,
    document: &SavedDocument,
    target: &PaperlessTarget,
    context: &DeliveryContext,
) -> Result<UploadMetadata> {
    let mut result = UploadMetadata {
        tag_ids: std::iter::once(target.inbox_tag_id)
            .chain(target.source_tag_id)
            .collect(),
        ..Default::default()
    };
    let Some(config) = &context.config else {
        return Ok(result);
    };
    let source = store.source_metadata(&document.letter_id)?;
    result.title = source.as_ref().and_then(|s| s.title.clone());
    result.tag_ids.extend(&config.extra_tag_ids);
    result.document_type = match &config.document_type {
        DocumentTypePolicy::PaperlessAutomatic => None,
        DocumentTypePolicy::Fixed(id) => Some(*id),
        DocumentTypePolicy::Mapping(mapping) => {
            let recognized: std::collections::BTreeSet<i64> = source
                .as_ref()
                .and_then(|s| s.document_types.as_ref())
                .into_iter()
                .flatten()
                .filter_map(|source| mapping.get(source).copied())
                .collect();
            if recognized.len() == 1 {
                recognized.into_iter().next()
            } else {
                None
            }
        }
    };
    for (id, value) in &config.custom_fields {
        let value = match value {
            MetadataValue::Literal(text) => Some(text.clone()),
            MetadataValue::Source(SourceField::LetterId) => Some(document.letter_id.clone()),
            MetadataValue::Source(SourceField::ReceivedAt) => {
                source.as_ref().and_then(|s| s.received_at.clone())
            }
            MetadataValue::Source(SourceField::OriginalFileName) => {
                source.as_ref().and_then(|s| s.original_file_name.clone())
            }
            MetadataValue::Source(SourceField::DocumentTypes) => source
                .as_ref()
                .and_then(|s| s.document_types.as_ref())
                .filter(|types| !types.is_empty())
                .map(|types| types.join(", ")),
        };
        if let Some(value) = value {
            result
                .custom_fields
                .insert(*id, serde_json::Value::String(value));
        }
    }
    Ok(result)
}

async fn validate_configured_metadata(
    client: &PaperlessClient,
    context: &DeliveryContext,
) -> Result<()> {
    let Some(config) = &context.config else {
        return Ok(());
    };
    if !client.tag(config.target.inbox_tag_id).await?.is_inbox_tag {
        return Err(Error::InvalidResponse(
            "configured Paperless inbox tag is not an inbox tag",
        ));
    }
    if let Some(id) = config.target.source_tag_id
        && client.tag(id).await?.is_inbox_tag
    {
        return Err(Error::InvalidResponse(
            "configured Paperless source tag must be a regular tag",
        ));
    }
    for id in config
        .extra_tag_ids
        .iter()
        .copied()
        .collect::<std::collections::BTreeSet<_>>()
    {
        let _ = client.tag(id).await?;
    }
    for id in config.custom_fields.keys() {
        if client.custom_field(*id).await?.data_type != "string" {
            return Err(Error::Config(
                "source metadata mappings require Paperless Text custom fields",
            ));
        }
    }
    let document_types: std::collections::BTreeSet<i64> = match &config.document_type {
        DocumentTypePolicy::PaperlessAutomatic => Default::default(),
        DocumentTypePolicy::Fixed(id) => [*id].into_iter().collect(),
        DocumentTypePolicy::Mapping(mapping) => mapping.values().copied().collect(),
    };
    for id in document_types {
        let _ = client.document_type(id).await?;
    }
    Ok(())
}

/// Check any retained history before saving a configuration, even after removal.
pub fn validate_destination_ledger(store: &Store, destination: &Destination) -> Result<()> {
    validate_destination(destination)?;
    match &destination.kind {
        DestinationKind::Paperless(config) => {
            let context = DeliveryContext::named(store, destination, false)?;
            let _ = load_ledger_for(store, &config.target, &context)?;
        }
        DestinationKind::Folder { .. } => {
            let _ = load_folder_ledger(store, destination)?;
        }
    }
    Ok(())
}

pub fn destination_status(store: &Store, destination: &Destination) -> Result<DeliveryStatus> {
    validate_destination_ledger(store, destination)?;
    match &destination.kind {
        DestinationKind::Paperless(config) => {
            let context = DeliveryContext::named(store, destination, false)?;
            let ledger = load_ledger_for(store, &config.target, &context)?;
            let documents = store.saved_documents()?;
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
        DestinationKind::Folder { .. } => {
            let ledger = load_folder_ledger(store, destination)?;
            let documents = store.saved_documents()?;
            let mut result = DeliveryStatus {
                saved_documents: documents.len(),
                ..Default::default()
            };
            for document in documents {
                if document.media_kind != "pdf" {
                    result.unsupported += 1;
                } else if ledger
                    .entries
                    .contains_key(&document_ref(store, &document.letter_id))
                {
                    result.verified += 1;
                } else {
                    result.not_submitted += 1;
                }
            }
            Ok(result)
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FolderLedger {
    schema_version: u32,
    namespace_hash: String,
    destination_identity: String,
    entries: BTreeMap<String, FolderRecord>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FolderRecord {
    sha256: String,
    bytes: u64,
}

fn load_folder_ledger(store: &Store, destination: &Destination) -> Result<FolderLedger> {
    let identity = destination_identity(store, destination);
    let ledger = match store.read_private_state(&ledger_name(destination))? {
        None => FolderLedger {
            schema_version: 1,
            namespace_hash: store.identity_hash().into(),
            destination_identity: identity.clone(),
            entries: BTreeMap::new(),
        },
        Some(raw) => serde_json::from_slice::<FolderLedger>(&raw)
            .map_err(|_| Error::Storage("folder delivery ledger is corrupt"))?,
    };
    if ledger.schema_version != 1
        || ledger.namespace_hash != store.identity_hash()
        || ledger.destination_identity != identity
        || ledger.entries.len() > MAX_ITEMS
    {
        return Err(Error::Storage(
            "folder ledger namespace, destination or version is invalid",
        ));
    }
    let inventory: BTreeMap<String, SavedDocument> = store
        .saved_documents()?
        .into_iter()
        .map(|d| (document_ref(store, &d.letter_id), d))
        .collect();
    for (reference, record) in &ledger.entries {
        let local = inventory.get(reference).ok_or(Error::Storage(
            "folder ledger refers to unknown local document",
        ))?;
        if local.media_kind != "pdf" || local.sha256 != record.sha256 || local.bytes != record.bytes
        {
            return Err(Error::Storage(
                "folder ledger conflicts with local document",
            ));
        }
    }
    Ok(ledger)
}

pub async fn deliver_folder(
    store: &mut Store,
    destination: &Destination,
    options: DeliveryOptions,
    progress: impl FnMut(DeliveryEvent),
) -> Result<DeliveryReport> {
    folder_inner(store, destination, options, false, progress).await
}

/// Verify existing copies; never copy an unsubmitted PDF during this audit.
pub async fn verify_folder(
    store: &mut Store,
    destination: &Destination,
    options: DeliveryOptions,
    progress: impl FnMut(DeliveryEvent),
) -> Result<DeliveryReport> {
    folder_inner(store, destination, options, true, progress).await
}

async fn folder_inner(
    store: &mut Store,
    destination: &Destination,
    options: DeliveryOptions,
    verify_only: bool,
    mut progress: impl FnMut(DeliveryEvent),
) -> Result<DeliveryReport> {
    validate_destination(destination)?;
    let DestinationKind::Folder { path } = &destination.kind else {
        return Err(Error::Config("destination is not a folder"));
    };
    if path == store.root_path() {
        return Err(Error::Config("folder destination is the source store"));
    }
    if options.max_documents == 0 || options.max_documents > MAX_ITEMS {
        return Err(Error::Config("folder delivery bounds are invalid"));
    }
    let mut ledger = load_folder_ledger(store, destination)?;
    let mut output = if verify_only && (ledger.entries.is_empty() || !path.exists()) {
        None
    } else {
        Some(Store::open(
            path,
            &format!(
                "epost-folder-v1:{}:{}",
                store.identity_hash(),
                destination_identity(store, destination)
            ),
        )?)
    };
    if output
        .as_ref()
        .is_some_and(|output| output.root_path() == store.root_path())
    {
        return Err(Error::Config("folder destination is the source store"));
    }
    let documents = store.saved_documents()?;
    if documents.len() > MAX_ITEMS {
        return Err(Error::Storage("folder inventory exceeds item bound"));
    }
    let mut report = DeliveryReport::default();
    let mut work = 0;
    progress(report.event(DeliveryPhase::Started, documents.len()));
    for document in &documents {
        let reference = document_ref(store, &document.letter_id);
        if crate::progress::stop_requested() {
            report.cancelled = true;
            report.add(item(
                &reference,
                DeliveryItemStatus::NotAttempted,
                None,
                Some("delivery stopped at a safe boundary; run again to continue"),
            ));
            continue;
        }
        if document.media_kind != "pdf" {
            report.add(item(
                &reference,
                DeliveryItemStatus::Unsupported,
                None,
                Some("only validated PDF files are supported for folder delivery"),
            ));
            continue;
        }
        if verify_only && !ledger.entries.contains_key(&reference) {
            report.add(item(
                &reference,
                DeliveryItemStatus::NotAttempted,
                None,
                Some("this PDF has not been copied to the destination; run check to deliver it"),
            ));
            continue;
        }
        if !ledger.entries.contains_key(&reference) {
            if work >= options.max_documents {
                report.add(item(
                    &reference,
                    DeliveryItemStatus::DeferredLimit,
                    None,
                    Some("delivery limit reached; run again to continue"),
                ));
                continue;
            }
            work += 1;
        }
        let bytes = match store.read_saved(&document.letter_id) {
            Ok(bytes) => bytes,
            Err(error) => {
                report.add(item_error(
                    &reference,
                    DeliveryItemStatus::Failed,
                    None,
                    &error,
                ));
                continue;
            }
        };
        progress(report.event(DeliveryPhase::Uploading, documents.len()));
        if verify_only {
            let copied = output
                .as_ref()
                .ok_or(Error::Storage("folder destination is missing"))
                .and_then(|output| output.read_saved(&document.letter_id));
            match copied {
                Ok(copy) if copy == bytes => report.add(item(
                    &reference,
                    DeliveryItemStatus::AlreadyDelivered,
                    None,
                    None,
                )),
                Ok(_) => report.add(item(
                    &reference,
                    DeliveryItemStatus::Failed,
                    None,
                    Some("folder copy does not match the local PDF"),
                )),
                Err(error) => report.add(item_error(
                    &reference,
                    DeliveryItemStatus::Failed,
                    None,
                    &error,
                )),
            }
            continue;
        }
        match output
            .as_mut()
            .ok_or(Error::Storage("folder destination is missing"))?
            .save(
                &document.letter_id,
                "epost",
                &bytes,
                Some("application/pdf"),
            ) {
            Ok(saved) => {
                let repeated = ledger.entries.contains_key(&reference);
                ledger.entries.insert(
                    reference.clone(),
                    FolderRecord {
                        sha256: saved.sha256,
                        bytes: saved.bytes,
                    },
                );
                persist(store, &ledger_name(destination), &ledger)?;
                report.add(item(
                    &reference,
                    if repeated {
                        DeliveryItemStatus::AlreadyDelivered
                    } else {
                        DeliveryItemStatus::Delivered
                    },
                    None,
                    None,
                ));
            }
            Err(error) => report.add(item_error(
                &reference,
                DeliveryItemStatus::Failed,
                None,
                &error,
            )),
        }
    }
    progress(report.event(DeliveryPhase::Finished, documents.len()));
    Ok(report)
}

fn item(
    reference: &str,
    status: DeliveryItemStatus,
    document_id: Option<i64>,
    error: Option<&str>,
) -> DeliveryItem {
    let (reason_code, recovery_action) = match status {
        DeliveryItemStatus::Pending => (
            Some(DeliveryReasonCode::Pending),
            Some(DeliveryRecoveryAction::Resume),
        ),
        DeliveryItemStatus::Uncertain => (
            Some(DeliveryReasonCode::UploadOutcomeUnknown),
            Some(DeliveryRecoveryAction::ReconcileExplicitly),
        ),
        DeliveryItemStatus::Failed => (
            Some(DeliveryReasonCode::VerificationFailed),
            Some(DeliveryRecoveryAction::RetryVerification),
        ),
        DeliveryItemStatus::Unsupported => (
            Some(DeliveryReasonCode::UnsupportedContent),
            Some(DeliveryRecoveryAction::ReviewSource),
        ),
        DeliveryItemStatus::DeferredLimit => (
            Some(DeliveryReasonCode::DeferredLimit),
            Some(DeliveryRecoveryAction::Resume),
        ),
        DeliveryItemStatus::NotAttempted => (
            Some(DeliveryReasonCode::NotAttempted),
            Some(DeliveryRecoveryAction::Resume),
        ),
        _ => (None, None),
    };
    DeliveryItem {
        document_ref: reference.to_owned(),
        status,
        document_id,
        error: error.map(str::to_owned),
        reason_code,
        recovery_action,
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
    let message = error.to_string();
    let mut result = item(reference, status, document_id, Some(&message));
    result.authentication_failed = matches!(error, Error::Auth | Error::Http(401 | 403));
    result.access_blocked = matches!(error, Error::PaperlessRedirect { .. });
    if result.authentication_failed {
        result.reason_code = Some(DeliveryReasonCode::AuthenticationFailed);
        result.recovery_action = Some(DeliveryRecoveryAction::RestoreAccess);
    } else if result.access_blocked {
        result.reason_code = Some(DeliveryReasonCode::AccessBlocked);
        result.recovery_action = Some(DeliveryRecoveryAction::CheckDestination);
    }
    result
}
