//! Named, secret-free destinations bound to one source mailbox.
use crate::delivery::{PaperlessTarget, load_target};
use crate::error::{Error, Result};
use crate::paperless::normalize_url;
use crate::storage::Store;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, PathBuf};

const STATE: &str = "destinations.json";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CredentialBinding {
    Legacy,
    Named,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Destination {
    pub name: String,
    pub credential_binding: CredentialBinding,
    pub kind: DestinationKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    tag = "type",
    content = "config",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum DestinationKind {
    Paperless(PaperlessDestination),
    Folder { path: PathBuf },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PaperlessDestination {
    pub target: PaperlessTarget,
    #[serde(default)]
    pub extra_tag_ids: Vec<i64>,
    #[serde(default)]
    pub custom_fields: BTreeMap<i64, MetadataValue>,
    #[serde(default)]
    pub document_type: DocumentTypePolicy,
}

impl From<PaperlessTarget> for PaperlessDestination {
    fn from(target: PaperlessTarget) -> Self {
        Self {
            target,
            extra_tag_ids: Vec::new(),
            custom_fields: BTreeMap::new(),
            document_type: DocumentTypePolicy::PaperlessAutomatic,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum MetadataValue {
    Literal(String),
    Source(SourceField),
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SourceField {
    LetterId,
    ReceivedAt,
    OriginalFileName,
    DocumentTypes,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum DocumentTypePolicy {
    #[default]
    PaperlessAutomatic,
    Fixed(i64),
    Mapping(BTreeMap<String, i64>),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Registry {
    schema_version: u32,
    namespace_hash: String,
    destinations: BTreeMap<String, Destination>,
}

pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 64
        || name == "."
        || name == ".."
        || !name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
    {
        return Err(Error::Config(
            "destination name must be 1..64 ASCII letters, digits, underscores or hyphens",
        ));
    }
    Ok(())
}

pub fn validate_destination(destination: &Destination) -> Result<()> {
    validate_name(&destination.name)?;
    if destination.credential_binding == CredentialBinding::Legacy
        && (destination.name != "paperless"
            || !matches!(destination.kind, DestinationKind::Paperless(_)))
    {
        return Err(Error::Config(
            "legacy credential binding is reserved for migrated paperless",
        ));
    }
    match &destination.kind {
        DestinationKind::Paperless(config) => {
            let target = &config.target;
            if target.schema_version != 1
                || normalize_url(&target.url, true)? != target.url
                || target.inbox_tag_id <= 0
                || target.source_tag_id.is_some_and(|id| id <= 0)
                || config.extra_tag_ids.len() > 98
                || config.extra_tag_ids.iter().any(|id| *id <= 0)
                || config.custom_fields.len() > 64
            {
                return Err(Error::Config(
                    "invalid Paperless destination metadata or target",
                ));
            }
            for (id, value) in &config.custom_fields {
                if *id <= 0 {
                    return Err(Error::Config("custom field IDs must be positive"));
                }
                if let MetadataValue::Literal(text) = value {
                    validate_text(text)?;
                }
            }
            match &config.document_type {
                DocumentTypePolicy::PaperlessAutomatic => (),
                DocumentTypePolicy::Fixed(id) if *id > 0 => (),
                DocumentTypePolicy::Mapping(mapping) if mapping.len() <= 64 => {
                    for (source, id) in mapping {
                        validate_text(source)?;
                        if *id <= 0 {
                            return Err(Error::Config("document type IDs must be positive"));
                        }
                    }
                }
                _ => return Err(Error::Config("invalid document type policy")),
            }
        }
        DestinationKind::Folder { path } => {
            if !path.is_absolute()
                || path.parent().is_none()
                || path.as_os_str().len() > 4096
                || path
                    .components()
                    .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
                || path
                    .to_str()
                    .is_none_or(|s| s.chars().any(char::is_control))
            {
                return Err(Error::Config(
                    "folder destination requires an absolute path without traversal",
                ));
            }
        }
    }
    Ok(())
}

fn validate_text(text: &str) -> Result<()> {
    if text.trim().is_empty() || text.len() > 4096 || text.chars().any(char::is_control) {
        return Err(Error::Config(
            "metadata text must be nonempty, bounded and contain no controls",
        ));
    }
    Ok(())
}

fn registry(store: &Store) -> Result<Registry> {
    let result = match store.read_private_state(STATE)? {
        Some(raw) => serde_json::from_slice::<Registry>(&raw)
            .map_err(|_| Error::Storage("destination configuration is corrupt"))?,
        None => {
            let mut destinations = BTreeMap::new();
            if store.read_private_state("paperless-target.json")?.is_some() {
                let legacy = Destination {
                    name: "paperless".into(),
                    credential_binding: CredentialBinding::Legacy,
                    kind: DestinationKind::Paperless(load_target(store)?.into()),
                };
                destinations.insert(legacy.name.clone(), legacy);
            }
            Registry {
                schema_version: 1,
                namespace_hash: store.identity_hash().to_owned(),
                destinations,
            }
        }
    };
    if result.schema_version != 1
        || result.namespace_hash != store.identity_hash()
        || result.destinations.len() > 64
    {
        return Err(Error::Storage(
            "destination configuration namespace, count or version is invalid",
        ));
    }
    for (name, destination) in &result.destinations {
        if name != &destination.name {
            return Err(Error::Storage(
                "destination configuration name does not match",
            ));
        }
        validate_destination(destination)?;
    }
    Ok(result)
}

pub fn list(store: &Store) -> Result<Vec<Destination>> {
    Ok(registry(store)?.destinations.into_values().collect())
}

pub fn load(store: &Store, name: &str) -> Result<Destination> {
    validate_name(name)?;
    registry(store)?
        .destinations
        .remove(name)
        .ok_or(Error::Config("destination is not configured"))
}

/// Empty selection means every configured destination, in name order.
pub fn select(store: &Store, names: &[String]) -> Result<Vec<Destination>> {
    let registry = registry(store)?;
    if names.is_empty() {
        return Ok(registry.destinations.into_values().collect());
    }
    let mut selected = Vec::new();
    let mut seen = BTreeSet::new();
    for name in names {
        validate_name(name)?;
        if !seen.insert(name) {
            return Err(Error::Config("destination selection contains a duplicate"));
        }
        selected.push(
            registry
                .destinations
                .get(name)
                .cloned()
                .ok_or(Error::Config("selected destination is not configured"))?,
        );
    }
    Ok(selected)
}

pub fn save(store: &mut Store, destination: &Destination) -> Result<()> {
    save_with_effect(store, destination, || Ok(()), |_| Ok(()))
}

pub fn save_with_effect<T>(
    store: &mut Store,
    destination: &Destination,
    effect: impl FnOnce() -> Result<T>,
    rollback: impl FnOnce(T) -> Result<()>,
) -> Result<()> {
    validate_destination(destination)?;
    if let DestinationKind::Folder { path } = &destination.kind
        && path == store.root_path()
    {
        return Err(Error::Config("folder destination is the source store"));
    }
    let mut registry = registry(store)?;
    if let Some(previous) = registry.destinations.get(&destination.name) {
        if previous.credential_binding != destination.credential_binding {
            return Err(Error::Config(
                "destination credential binding cannot be changed",
            ));
        }
    } else if destination.credential_binding == CredentialBinding::Legacy {
        let legacy = load_target(store)?;
        let DestinationKind::Paperless(config) = &destination.kind else {
            unreachable!()
        };
        if config.target.url != legacy.url {
            return Err(Error::Config(
                "legacy destination must retain its existing Paperless instance",
            ));
        }
    }
    if destination.name == "paperless"
        && destination.credential_binding == CredentialBinding::Named
        && store
            .read_private_state("paperless-delivery.json")?
            .is_some()
    {
        return Err(Error::Config(
            "the paperless name has retained legacy history; use its legacy binding or choose a new destination name",
        ));
    }
    crate::delivery::validate_destination_ledger(store, destination)?;
    registry
        .destinations
        .insert(destination.name.clone(), destination.clone());
    if registry.destinations.len() > 64 {
        return Err(Error::Config("at most 64 destinations can be configured"));
    }
    let raw = serde_json::to_vec_pretty(&registry)
        .map_err(|_| Error::Storage("destinations cannot be serialized"))?;
    store.write_private_state_with_effect(STATE, &raw, effect, rollback)
}

pub fn remove(store: &mut Store, name: &str) -> Result<()> {
    remove_with_effect(store, name, || Ok(()), |_| Ok(()))
}

pub fn remove_with_effect<T>(
    store: &mut Store,
    name: &str,
    effect: impl FnOnce() -> Result<T>,
    rollback: impl FnOnce(T) -> Result<()>,
) -> Result<()> {
    validate_name(name)?;
    let mut registry = registry(store)?;
    registry
        .destinations
        .remove(name)
        .ok_or(Error::Config("destination is not configured"))?;
    let raw = serde_json::to_vec_pretty(&registry)
        .map_err(|_| Error::Storage("destinations cannot be serialized"))?;
    store.write_private_state_with_effect(STATE, &raw, effect, rollback)
}

/// Ledger filename depends only on the name; the contents pin endpoint identity,
/// preventing a name from silently adopting another instance's history.
pub fn ledger_name(destination: &Destination) -> String {
    if destination.credential_binding == CredentialBinding::Legacy {
        return "paperless-delivery.json".into();
    }
    format!(
        "destination-{}.json",
        hex::encode(Sha256::digest(destination.name.as_bytes()))
    )
}

pub fn destination_identity(store: &Store, destination: &Destination) -> String {
    let mut hash = Sha256::new();
    hash.update(b"epost-destination-v1\0");
    hash.update(store.root_path().as_os_str().as_encoded_bytes());
    hash.update(b"\0");
    hash.update(store.identity_hash().as_bytes());
    hash.update(b"\0");
    hash.update(destination.name.as_bytes());
    hash.update(b"\0");
    match &destination.kind {
        DestinationKind::Paperless(config) => {
            hash.update(b"paperless\0");
            hash.update(config.target.url.as_bytes());
        }
        DestinationKind::Folder { path } => {
            hash.update(b"folder\0");
            hash.update(path.as_os_str().as_encoded_bytes());
        }
    }
    hex::encode(hash.finalize())
}
