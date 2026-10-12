//! Guided configuration and credential isolation for named destinations.
use super::*;
use epost_cli::{
    destinations::{
        self, CredentialBinding, Destination, DestinationKind, DocumentTypePolicy, MetadataValue,
        PaperlessDestination, SourceField,
    },
    keychain::CredentialStore,
    paperless::{PaperlessClient, Tag, normalize_url},
};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Subcommand, Clone)]
pub(super) enum DestinationCommand {
    /// Add a Paperless instance or a verified local folder copy.
    Add {
        name: Option<String>,
        #[arg(long)]
        folder: Option<PathBuf>,
        #[arg(long)]
        url: Option<String>,
    },
    /// Show all destinations; every configured destination participates by default.
    List,
    /// Reconfigure tags and metadata, retaining delivery history.
    Edit { name: String },
    /// Remove configuration and saved token; retain documents and delivery history.
    Remove {
        name: String,
        #[arg(long)]
        yes: bool,
    },
}

pub(super) fn open_store(cli: &Cli, explicit: Option<&Path>) -> Result<(PathBuf, Store)> {
    let root = output(cli, explicit)?;
    let store = Store::open(&root, &namespace(cli, &mailbox(cli)?)?)?;
    Ok((root, store))
}

fn account(root: &Path, store: &Store, destination: &Destination) -> Result<String> {
    match &destination.kind {
        DestinationKind::Paperless(config)
            if destination.credential_binding == CredentialBinding::Legacy =>
        {
            paperless_cli::token_account(root, store, &config.target.url)
        }
        DestinationKind::Paperless(_) => Ok(destinations::destination_identity(store, destination)),
        _ => Err(Error::Config("folder destinations do not have tokens")),
    }
}

fn token_env_name(name: &str) -> String {
    // Encode punctuation rather than replacing it: distinct names must never share a token.
    format!(
        "EPOST_DESTINATION_{}_TOKEN",
        hex::encode(name.as_bytes()).to_ascii_uppercase()
    )
}

fn preparation_identity(root: &Path, store: &Store, destination: &Destination) -> Result<String> {
    // Include the entire reviewed secret-free configuration, not merely its
    // name. The store lock is released between prompts and source retrieval;
    // another operation may legitimately edit a destination during that gap.
    let snapshot = serde_json::to_vec(&(
        "epost-named-prepared-token-v1",
        root,
        store.root_path(),
        store.identity_hash(),
        destination,
    ))
    .map_err(|_| Error::Config("cannot construct prepared destination identity"))?;
    Ok(hex::encode(Sha256::digest(snapshot)))
}

fn token(cli: &Cli, root: &Path, store: &Store, destination: &Destination) -> Result<Secret> {
    if cli
        .prepared_destination_failures
        .contains(&destination.name)
    {
        return Err(Error::Config(
            "destination credential preparation failed or was cancelled; retry the command or run destination edit",
        ));
    }
    if let Some(token) = cli.prepared_destination_tokens.get(&destination.name) {
        return token.get(&preparation_identity(root, store, destination)?);
    }
    if cli.destinations_prepared {
        // A newly added or renamed target was never reviewed in this operation.
        // Never fall back to a generic token that was supplied for another host.
        return Err(Error::Config(paperless_cli::PREPARATION_CHANGED));
    }
    let DestinationKind::Paperless(config) = &destination.kind else {
        return Err(Error::Config(
            "folder destinations do not require credentials",
        ));
    };
    if let Some(token) = env_secret(&token_env_name(&destination.name))? {
        return paperless_cli::checked_token(token.expose().to_owned());
    }
    let count = destinations::list(store)?
        .iter()
        .filter(|d| matches!(d.kind, DestinationKind::Paperless(_)))
        .count();
    if cli.paperless_token_file.is_some() || std::env::var_os("PAPERLESS_TOKEN").is_some() {
        if count > 1 {
            return Err(Error::Config(
                "multiple Paperless destinations require separately saved tokens or per-destination token environment variables; generic token input is ambiguous",
            ));
        }
        if let Some(path) = &cli.paperless_token_file {
            if std::env::var_os("PAPERLESS_TOKEN").is_some() {
                return Err(Error::Config(
                    "choose PAPERLESS_TOKEN or --paperless-token-file, not both",
                ));
            }
            return paperless_cli::file_token(path);
        }
        return paperless_cli::checked_token(
            env_secret("PAPERLESS_TOKEN")?
                .ok_or(Error::Config("Paperless token missing"))?
                .expose()
                .to_owned(),
        );
    }
    if config.target.remember_token {
        native_keychain_allowed(cli)?;
        let id = account(root, store, destination)?;
        let secret = if cli.non_interactive {
            keychain::NativeStore.get_noninteractive(paperless_cli::TOKEN_SERVICE, &id)
        } else {
            keychain::NativeStore.get(paperless_cli::TOKEN_SERVICE, &id)
        }?;
        return paperless_cli::checked_token(secret.expose().to_owned());
    }
    if cli.non_interactive || cli.json || !io::stdin().is_terminal() {
        return Err(Error::Config(
            "destination token unavailable; run destination edit to remember it or supply a per-destination runtime token",
        ));
    }
    let mut dialog = Dialog::new(cli)?;
    paperless_cli::checked_token(dialog.required(
        &format!("Paperless token for {} (hidden): ", destination.name),
        true,
    )?)
}

pub(super) fn client_for(
    cli: &Cli,
    root: &Path,
    store: &Store,
    destination: &Destination,
) -> Result<PaperlessClient> {
    let DestinationKind::Paperless(config) = &destination.kind else {
        return Err(Error::Config(
            "Paperless client requires a Paperless destination",
        ));
    };
    paperless_cli::make_client(
        cli,
        &config.target.url,
        token(cli, root, store, destination)?,
    )
}

pub(super) fn prepare(cli: &mut Cli, names: &[String], explicit: Option<&Path>) -> Result<()> {
    prepare_with(cli, names, explicit, token)
}

fn prepare_with(
    cli: &mut Cli,
    names: &[String],
    explicit: Option<&Path>,
    mut load: impl FnMut(&Cli, &Path, &Store, &Destination) -> Result<Secret>,
) -> Result<()> {
    cli.prepared_destination_tokens.clear();
    cli.prepared_destination_failures.clear();
    cli.destinations_prepared = false;
    let (root, store) = open_store(cli, explicit)?;
    for destination in destinations::select(&store, names)? {
        if matches!(destination.kind, DestinationKind::Paperless(_)) {
            // Keep failures isolated: the workflow emits a destination error and continues.
            match load(cli, &root, &store, &destination) {
                Ok(secret) => {
                    let prepared = paperless_cli::PreparedToken::new(
                        preparation_identity(&root, &store, &destination)?,
                        secret,
                    );
                    cli.prepared_destination_tokens
                        .insert(destination.name.clone(), prepared);
                }
                Err(_) => {
                    cli.prepared_destination_failures
                        .insert(destination.name.clone());
                }
            }
        }
    }
    cli.destinations_prepared = true;
    Ok(())
}

pub(super) async fn validate_paperless_catalogs(
    client: &PaperlessClient,
    config: &PaperlessDestination,
) -> Result<()> {
    let inbox = client.tag(config.target.inbox_tag_id).await?;
    if !inbox.is_inbox_tag {
        return Err(Error::Config(
            "configured inbox tag no longer has the Paperless inbox flag; run destination edit",
        ));
    }
    if let Some(id) = config.target.source_tag_id
        && client.tag(id).await?.is_inbox_tag
    {
        return Err(Error::Config("source tag must not be an inbox tag"));
    }
    for id in &config.extra_tag_ids {
        let _ = client.tag(*id).await?;
    }
    let ids: BTreeSet<i64> = match &config.document_type {
        DocumentTypePolicy::PaperlessAutomatic => BTreeSet::new(),
        DocumentTypePolicy::Fixed(id) => BTreeSet::from([*id]),
        DocumentTypePolicy::Mapping(mapping) => mapping.values().copied().collect(),
    };
    for id in ids {
        let _ = client.document_type(id).await?;
    }
    for id in config.custom_fields.keys() {
        if client.custom_field(*id).await?.data_type != "string" {
            return Err(Error::Config(
                "provenance custom fields must have Text type; run destination edit",
            ));
        }
    }
    Ok(())
}

struct Dialog {
    tui: Option<tui::TuiPrompts>,
}
impl Dialog {
    fn new(cli: &Cli) -> Result<Self> {
        if cli.json || cli.non_interactive || !io::stdin().is_terminal() {
            return Err(Error::Config(
                "destination setup needs an interactive terminal; omit --json and --non-interactive",
            ));
        }
        Ok(Self {
            tui: if tui::available() {
                Some(tui::TuiPrompts::new()?)
            } else {
                None
            },
        })
    }
    fn prompt(&mut self, prompt: &str, secret: bool) -> Result<Option<String>> {
        if let Some(ui) = &mut self.tui {
            return ui.prompt(prompt, secret);
        }
        if secret {
            return Ok(Some(rpassword::prompt_password(interactive::safe_line(
                prompt,
            ))?));
        }
        use interactive::Prompts;
        interactive::Terminal::new()?.input(&interactive::safe_line(prompt))
    }
    fn required(&mut self, prompt: &str, secret: bool) -> Result<String> {
        self.prompt(prompt, secret)?
            .ok_or(Error::Config("setup cancelled"))
    }
    fn default(&mut self, prompt: &str, default: &str) -> Result<String> {
        let answer = self.required(&format!("{prompt} [{default}]: "), false)?;
        Ok(if answer.trim().is_empty() {
            default.into()
        } else {
            answer.trim().into()
        })
    }
    fn yes(&mut self, prompt: &str, default: bool) -> Result<bool> {
        loop {
            match self
                .default(prompt, if default { "yes" } else { "no" })?
                .to_ascii_lowercase()
                .as_str()
            {
                "yes" | "y" => return Ok(true),
                "no" | "n" => return Ok(false),
                _ => self.line("Choose yes or no; please try again.")?,
            }
        }
    }
    fn line(&mut self, line: &str) -> Result<()> {
        if let Some(ui) = &mut self.tui {
            interactive::Prompts::line(ui, line)
        } else {
            writeln!(io::stderr().lock(), "{}", interactive::safe_line(line)).map_err(Error::from)
        }
    }
}

async fn choose_tag(
    ui: &mut Dialog,
    client: &PaperlessClient,
    suggested: &str,
    inbox: bool,
) -> Result<Tag> {
    loop {
        let name = ui.default(
            if inbox {
                "Inbox tag name (must have inbox flag)"
            } else {
                "Source tag name"
            },
            suggested,
        )?;
        let tags = client.tags().await?;
        let matches: Vec<_> = tags.iter().filter(|t| t.name == name).collect();
        match matches.as_slice() {
            [tag] if tag.is_inbox_tag == inbox => return Ok((*tag).clone()),
            [_, _, ..] => ui.line("Duplicate names in Paperless; choose a unique name.")?,
            [_] => ui.line(if inbox { "This existing tag has no inbox flag. Enable it in Paperless or choose a different name." } else { "This tag is an inbox tag. Choose a regular source tag." })?,
            [] => {
                if name.trim().is_empty() || name.len() > 128 || name.chars().any(char::is_control) { ui.line("Choose a nonempty tag name under 128 bytes.")?; continue; }
                if ui.yes("Create this tag in Paperless?", true)? { return client.create_tag(&name, inbox).await; }
            }
        }
    }
}

async fn configure_metadata(
    ui: &mut Dialog,
    client: &PaperlessClient,
    config: &mut PaperlessDestination,
) -> Result<()> {
    ui.line("Extra tags apply to future uploads. Existing reviewed documents are not retagged.")?;
    if ui.yes(
        "Configure additional tags?",
        !config.extra_tag_ids.is_empty(),
    )? {
        config.extra_tag_ids.clear();
        loop {
            let name = ui.required("Extra tag name (Enter finishes): ", false)?;
            if name.is_empty() {
                break;
            }
            let tags = client.tags().await?;
            let matches: Vec<_> = tags.iter().filter(|t| t.name == name).collect();
            let id = match matches.as_slice() {
                [tag] => tag.id,
                [] if ui.yes("Create regular tag?", true)? => {
                    client.create_tag(&name, false).await?.id
                }
                [] => continue,
                _ => {
                    ui.line("Duplicate tag name; choose a unique name.")?;
                    continue;
                }
            };
            if !config.extra_tag_ids.contains(&id) {
                config.extra_tag_ids.push(id);
            }
            if config.extra_tag_ids.len() >= 98 {
                break;
            }
        }
    } else {
        config.extra_tag_ids.clear();
    }
    ui.line("Document type: 1 = let Paperless classify, 2 = fixed type, 3 = explicit ePost type mapping.")?;
    ui.line("Recommended: create Invoice, Statement, Contract and Correspondence in Paperless, with Auto matching. Review the first documents, correct their type, then remove inbox so Paperless can learn.")?;
    loop {
        let policy = ui.default("Document type policy", "1")?;
        match policy.as_str() {
            "1" => {
                config.document_type = DocumentTypePolicy::PaperlessAutomatic;
                break;
            }
            "2" | "3" => {
                let types = client.document_types().await?;
                for t in &types {
                    ui.line(&format!("  {} = {}", t.id, interactive::safe_line(&t.name)))?;
                }
                ui.line("Types must already exist in Paperless. Missing or ambiguous ePost types are omitted.")?;
                let value = ui.required(
                    "Fixed numeric ID, or mapping like invoice=1,contract=2: ",
                    false,
                )?;
                if policy == "2" {
                    if let Ok(id) = value.parse::<i64>()
                        && types.iter().any(|t| t.id == id)
                    {
                        config.document_type = DocumentTypePolicy::Fixed(id);
                        break;
                    }
                } else {
                    let mut mapping = BTreeMap::new();
                    let mut valid = true;
                    for pair in value.split(',') {
                        if let Some((source, id)) = pair.split_once('=')
                            && let Ok(id) = id.trim().parse::<i64>()
                            && !source.trim().is_empty()
                            && types.iter().any(|t| t.id == id)
                            && mapping.insert(source.trim().into(), id).is_none()
                        {
                            continue;
                        }
                        valid = false;
                    }
                    if valid && !mapping.is_empty() {
                        config.document_type = DocumentTypePolicy::Mapping(mapping);
                        break;
                    }
                }
                ui.line("Invalid type selection; please try again.")?;
            }
            _ => ui.line("Choose 1, 2 or 3.")?,
        }
    }
    if ui.yes(
        "Configure provenance custom fields? (optional Text fields)",
        !config.custom_fields.is_empty(),
    )? {
        config.custom_fields.clear();
        for (source, suggested, description) in [
            (SourceField::LetterId, "ePost ID", "ePost document ID"),
            (
                SourceField::ReceivedAt,
                "ePost received at",
                "received timestamp, not document issue date",
            ),
            (
                SourceField::OriginalFileName,
                "ePost filename",
                "original filename",
            ),
            (
                SourceField::DocumentTypes,
                "ePost document types",
                "raw upstream document types",
            ),
        ] {
            if !ui.yes(&format!("Include {description}?"), false)? {
                continue;
            }
            let id = choose_text_field(ui, client, suggested).await?;
            if config
                .custom_fields
                .insert(id, MetadataValue::Source(source))
                .is_some()
            {
                return Err(Error::Config(
                    "each metadata mapping needs its own custom field",
                ));
            }
        }
        while ui.yes("Add a static Text field value?", false)? {
            let id = choose_text_field(ui, client, "Source system").await?;
            let value = ui.required("Static text value: ", false)?;
            if config
                .custom_fields
                .insert(id, MetadataValue::Literal(value))
                .is_some()
            {
                return Err(Error::Config(
                    "each metadata mapping needs its own custom field",
                ));
            }
            if config.custom_fields.len() >= 64 {
                break;
            }
        }
    } else {
        config.custom_fields.clear();
    }
    Ok(())
}

async fn choose_text_field(
    ui: &mut Dialog,
    client: &PaperlessClient,
    suggestion: &str,
) -> Result<i64> {
    loop {
        let name = ui.default("Custom Text field name", suggestion)?;
        let fields = client.custom_fields().await?;
        let matches: Vec<_> = fields.iter().filter(|f| f.name == name).collect();
        match matches.as_slice() {
            [field] if field.data_type == "string" => return Ok(field.id),
            [] if ui.yes("Create Text field in Paperless?", true)? => {
                return Ok(client.create_custom_field(&name).await?.id);
            }
            [] => (),
            _ => ui.line(
                "Existing field must have Text type and a unique name. Choose another name.",
            )?,
        }
    }
}

fn persist(
    cli: &Cli,
    root: &Path,
    store: &mut Store,
    destination: &Destination,
    secret: Option<&Secret>,
) -> Result<()> {
    let previous = destinations::list(store)?
        .into_iter()
        .find(|d| d.name == destination.name);
    let mut changes = BTreeMap::new();
    if let Some(old) = previous.as_ref()
        && let DestinationKind::Paperless(config) = &old.kind
        && config.target.remember_token
    {
        changes.insert(account(root, store, old)?, None);
    }
    if let DestinationKind::Paperless(config) = &destination.kind
        && config.target.remember_token
    {
        changes.insert(
            account(root, store, destination)?,
            Some(
                secret
                    .ok_or(Error::Config("missing token to remember"))?
                    .clone(),
            ),
        );
    }
    if changes.is_empty() {
        return destinations::save(store, destination);
    }
    native_keychain_mutation_allowed(cli)?;
    destinations::save_with_effect(
        store,
        destination,
        || {
            let mut snapshot = Vec::new();
            for id in changes.keys() {
                snapshot.push((
                    id.clone(),
                    keychain::NativeStore.get_optional(paperless_cli::TOKEN_SERVICE, id)?,
                ));
            }
            for (id, value) in &changes {
                let result = match value {
                    Some(value) => {
                        keychain::NativeStore.set(paperless_cli::TOKEN_SERVICE, id, value)
                    }
                    None => keychain::NativeStore.delete(paperless_cli::TOKEN_SERVICE, id),
                };
                if let Err(error) = result {
                    restore(snapshot)?;
                    return Err(error);
                }
            }
            Ok(snapshot)
        },
        restore,
    )
}
fn restore(snapshot: Vec<(String, Option<Secret>)>) -> Result<()> {
    for (id, value) in snapshot {
        match value {
            Some(token) => keychain::NativeStore.set(paperless_cli::TOKEN_SERVICE, &id, &token)?,
            None => keychain::NativeStore.delete(paperless_cli::TOKEN_SERVICE, &id)?,
        }
    }
    Ok(())
}

pub(super) async fn run(cli: &Cli, command: &DestinationCommand) -> Result<(Value, u8)> {
    let (root, mut store) = open_store(cli, None)?;
    match command {
        DestinationCommand::List => {
            let destinations = destinations::list(&store)?.into_iter().map(|d| json!({"name": d.name, "kind": if matches!(d.kind, DestinationKind::Paperless(_)) { "paperless" } else { "folder" }, "used_by_default": true})).collect::<Vec<_>>();
            Ok((json!({"destinations": destinations}), 0))
        }
        DestinationCommand::Remove { name, yes } => {
            let existing = destinations::load(&store, name)?;
            if !yes && !Dialog::new(cli)?.yes("Remove destination configuration and its saved token? Documents/history are retained.", false)? { return Ok((json!({"cancelled": true}), 0)); }
            if let DestinationKind::Paperless(config) = &existing.kind {
                if config.target.remember_token {
                    native_keychain_mutation_allowed(cli)?;
                    let id = account(&root, &store, &existing)?;
                    destinations::remove_with_effect(
                        &mut store,
                        name,
                        || {
                            let old = keychain::NativeStore
                                .get_optional(paperless_cli::TOKEN_SERVICE, &id)?;
                            keychain::NativeStore.delete(paperless_cli::TOKEN_SERVICE, &id)?;
                            Ok(vec![(id.clone(), old)])
                        },
                        restore,
                    )?;
                } else {
                    destinations::remove(&mut store, name)?;
                }
            } else {
                destinations::remove(&mut store, name)?;
            }
            Ok((json!({"removed": name, "history_retained": true}), 0))
        }
        DestinationCommand::Add {
            name,
            folder: Some(path),
            url: None,
        } => {
            let name = name.clone().ok_or(Error::Config(
                "supply a name for noninteractive folder setup",
            ))?;
            if destinations::list(&store)?.iter().any(|d| d.name == name) {
                return Err(Error::Config(
                    "destination already exists; use destination edit",
                ));
            }
            let destination = Destination {
                name,
                credential_binding: CredentialBinding::Named,
                kind: DestinationKind::Folder { path: path.clone() },
            };
            destinations::save(&mut store, &destination)?;
            Ok((
                json!({"configured": destination.name, "kind": "folder", "used_by_default": true}),
                0,
            ))
        }
        DestinationCommand::Add { name, folder, url } => {
            if folder.is_some() {
                return Err(Error::Config("choose --folder or --url, not both"));
            }
            let mut ui = Dialog::new(cli)?;
            let name = match name {
                Some(name) => name.clone(),
                None => ui.default("Destination name", "paperless")?,
            };
            destinations::validate_name(&name)?;
            if destinations::list(&store)?.iter().any(|d| d.name == name) {
                return Err(Error::Config(
                    "destination already exists; use destination edit",
                ));
            }
            let kind = if url.is_some() {
                "paperless".into()
            } else {
                loop {
                    let candidate = ui
                        .default("Destination kind (paperless/folder)", "paperless")?
                        .to_ascii_lowercase();
                    if candidate == "paperless" || candidate == "folder" {
                        break candidate;
                    }
                    ui.line("Choose paperless or folder; please try again.")?;
                }
            };
            if kind == "folder" {
                let path = PathBuf::from(ui.required("Absolute folder path: ", false)?);
                let destination = Destination {
                    name,
                    credential_binding: CredentialBinding::Named,
                    kind: DestinationKind::Folder { path },
                };
                destinations::save(&mut store, &destination)?;
                return Ok((
                    json!({"configured": destination.name, "kind": "folder", "used_by_default": true}),
                    0,
                ));
            }
            if kind != "paperless" {
                return Err(Error::Config(
                    "destination kind must be paperless or folder",
                ));
            }
            let url = normalize_url(
                &match url {
                    Some(url) => url.clone(),
                    None => ui.required("Paperless HTTPS URL: ", false)?,
                },
                cli.mock_url.is_some(),
            )?;
            let credential_binding = new_paperless_binding(&store, &name, &url)?;
            configure_paperless(
                cli,
                &root,
                &mut store,
                &mut ui,
                Destination {
                    name,
                    credential_binding,
                    kind: DestinationKind::Paperless(PaperlessDestination::from(
                        epost_cli::delivery::PaperlessTarget {
                            schema_version: 1,
                            url,
                            inbox_tag_id: 1,
                            source_tag_id: None,
                            remember_token: false,
                        },
                    )),
                },
                false,
            )
            .await
        }
        DestinationCommand::Edit { name } => {
            let existing = destinations::load(&store, name)?;
            let mut ui = Dialog::new(cli)?;
            if matches!(existing.kind, DestinationKind::Folder { .. }) {
                return Err(Error::Config(
                    "folder path is pinned to its history; remove this destination and add a new name to change its path",
                ));
            }
            configure_paperless(cli, &root, &mut store, &mut ui, existing, true).await
        }
    }
}

fn new_paperless_binding(store: &Store, name: &str, url: &str) -> Result<CredentialBinding> {
    if name == "paperless" && store.read_private_state("paperless-target.json")?.is_some() {
        let legacy = epost_cli::delivery::load_target(store)?;
        if legacy.url != url {
            return Err(Error::Config(
                "this name retains the previous Paperless instance; choose a new destination name for another URL",
            ));
        }
        return Ok(CredentialBinding::Legacy);
    }
    Ok(CredentialBinding::Named)
}

fn setup_token(
    cli: &Cli,
    store: &Store,
    destination: &Destination,
    ui: &mut Dialog,
) -> Result<Secret> {
    if let Some(value) = env_secret(&token_env_name(&destination.name))? {
        return paperless_cli::checked_token(value.expose().to_owned());
    }
    if cli.paperless_token_file.is_some() && std::env::var_os("PAPERLESS_TOKEN").is_some() {
        return Err(Error::Config(
            "choose PAPERLESS_TOKEN or --paperless-token-file, not both",
        ));
    }
    let generic =
        cli.paperless_token_file.is_some() || std::env::var_os("PAPERLESS_TOKEN").is_some();
    if generic
        && destinations::list(store)?
            .iter()
            .any(|d| d.name != destination.name && matches!(d.kind, DestinationKind::Paperless(_)))
    {
        return Err(Error::Config(
            "adding another Paperless instance requires a named token or hidden setup prompt; generic token is ambiguous",
        ));
    }
    if let Some(path) = &cli.paperless_token_file {
        return paperless_cli::file_token(path);
    }
    if let Some(value) = env_secret("PAPERLESS_TOKEN")? {
        return paperless_cli::checked_token(value.expose().to_owned());
    }
    paperless_cli::checked_token(ui.required("Paperless API token (hidden): ", true)?)
}

async fn configure_paperless(
    cli: &Cli,
    root: &Path,
    store: &mut Store,
    ui: &mut Dialog,
    mut destination: Destination,
    editing: bool,
) -> Result<(Value, u8)> {
    epost_cli::delivery::validate_destination_ledger(store, &destination)?;
    let secret = if editing && !ui.yes("Replace the current token?", false)? {
        token(cli, root, store, &destination)?
    } else {
        setup_token(cli, store, &destination, ui)?
    };
    let DestinationKind::Paperless(config) = &mut destination.kind else {
        unreachable!()
    };
    let client = paperless_cli::make_client(cli, &config.target.url, secret.clone())?;
    let existing = client.tags().await?;
    ui.line("Use two separate tags: epost is the permanent source; inbox is removed after you process the document. Repeat runs do not restore removed review tags.")?;
    let inbox_suggestion = existing
        .iter()
        .find(|t| editing && t.id == config.target.inbox_tag_id)
        .map(|t| t.name.as_str())
        .unwrap_or("inbox");
    config.target.inbox_tag_id = choose_tag(ui, &client, inbox_suggestion, true).await?.id;
    let source_suggestion = existing
        .iter()
        .find(|t| editing && Some(t.id) == config.target.source_tag_id)
        .map(|t| t.name.as_str())
        .unwrap_or("epost");
    config.target.source_tag_id = Some(choose_tag(ui, &client, source_suggestion, false).await?.id);
    if !editing || ui.yes("Replace extra tags, type policy and custom field mappings? Existing settings are kept if no.", false)? { configure_metadata(ui, &client, config).await?; }
    config.target.remember_token = if keychain::supported() && cli.mock_url.is_none() {
        ui.yes("Remember token in this Mac's Keychain?", true)?
    } else {
        false
    };
    validate_paperless_catalogs(&client, config).await?;
    persist(cli, root, store, &destination, Some(&secret))?;
    Ok((
        json!({"configured": destination.name, "kind":"paperless", "used_by_default":true, "metadata_optional":true}),
        0,
    ))
}

pub(super) fn summary(report: &Value) -> Vec<String> {
    if let Some(name) = report["configured"].as_str() {
        return vec![
            format!(
                "Destination {name} configured. Every configured destination is used by default."
            ),
            "Run epost check inbox to fetch and deliver; ePost remains read-only.".into(),
        ];
    }
    if let Some(name) = report["removed"].as_str() {
        return vec![format!(
            "Destination {name} removed. Documents and delivery history retained."
        )];
    }
    let Some(destinations) = report["destinations"].as_array() else {
        return vec!["Configuration cancelled.".into()];
    };
    if destinations.is_empty() {
        return vec![
            "No destinations configured. Local downloads are retained. Run epost destination add."
                .into(),
        ];
    }
    destinations
        .iter()
        .map(|d| {
            format!(
                "{}: {} (used by default)",
                d["name"].as_str().unwrap_or("?"),
                d["kind"].as_str().unwrap_or("?")
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header, method, path},
    };

    fn prepared_cli(root: &Path, url: &str) -> Cli {
        Cli::parse_from([
            "epost",
            "--non-interactive",
            "--json",
            "--mock-url",
            url,
            "--tenant-kind",
            "private",
            "--tenant-id",
            "synthetic-prepared-tenant",
            "--company-id",
            "0",
            "deliver",
            "--output",
            root.to_str().unwrap(),
        ])
    }

    fn prepared_destination(url: &str) -> Destination {
        Destination {
            name: "synthetic-target".into(),
            credential_binding: CredentialBinding::Named,
            kind: DestinationKind::Paperless(
                epost_cli::delivery::PaperlessTarget {
                    schema_version: 1,
                    url: format!("{url}/"),
                    inbox_tag_id: 1,
                    source_tag_id: Some(2),
                    remember_token: false,
                }
                .into(),
            ),
        }
    }

    fn prepare_synthetic(cli: &mut Cli, source: &Path) {
        prepare_with(cli, &[], Some(source), |_, _, _, _| {
            Ok(Secret::new("synthetic-prepared-token".into()))
        })
        .unwrap();
    }

    #[tokio::test]
    async fn prepared_named_token_rejects_changed_target_store_and_namespace_without_requests() {
        let original = MockServer::start().await;
        let changed = MockServer::start().await;
        for scenario in ["url", "tags", "output", "namespace", "renamed"] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().canonicalize().unwrap();
            let source = root.join("source");
            let mut cli = prepared_cli(&source, &original.uri());
            cli.paperless_token_file = Some(root.join("synthetic-token-must-not-be-read"));
            let mut destination = prepared_destination(&original.uri());
            {
                let (_, mut store) = open_store(&cli, Some(&source)).unwrap();
                destinations::save(&mut store, &destination).unwrap();
            }
            prepare_synthetic(&mut cli, &source);
            assert!(cli.destinations_prepared);
            assert_eq!(cli.prepared_destination_tokens.len(), 1);
            let new_source = if scenario == "output" {
                root.join("another-source")
            } else {
                source.clone()
            };
            if scenario == "namespace" {
                cli.tenant_id = Some("synthetic-other-tenant".into());
            }
            if let DestinationKind::Paperless(config) = &mut destination.kind {
                if scenario == "url" || scenario == "renamed" {
                    config.target.url = format!("{}/", changed.uri());
                }
                if scenario == "tags" {
                    config.target.inbox_tag_id = 3;
                }
            }
            {
                let (_, mut store) = open_store(&cli, Some(&new_source)).unwrap();
                if scenario == "renamed" {
                    destinations::remove(&mut store, &destination.name).unwrap();
                    destination.name = "synthetic-renamed".into();
                }
                destinations::save(&mut store, &destination).unwrap();
            }
            if let Command::Deliver(args) = &mut cli.command {
                args.output = Some(new_source);
            }
            let (report, code) = modern::run(&cli).await.unwrap();
            assert_eq!(code, 1, "scenario {scenario}");
            assert_eq!(
                report["destinations"][0]["error"],
                Error::Config(paperless_cli::PREPARATION_CHANGED).to_string()
            );
            assert!(original.received_requests().await.unwrap().is_empty());
            assert!(changed.received_requests().await.unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn prepared_named_token_supports_unchanged_target_and_preserves_cancelled_preparation() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/tags/"))
            .and(header("authorization", "Token synthetic-prepared-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .expect(1)
            .mount(&server)
            .await;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let source = root.join("source");
        let mut cli = prepared_cli(&source, &server.uri());
        cli.paperless_token_file = Some(root.join("synthetic-token-must-not-be-read"));
        let destination = prepared_destination(&server.uri());
        {
            let (_, mut store) = open_store(&cli, Some(&source)).unwrap();
            destinations::save(&mut store, &destination).unwrap();
        }
        prepare_synthetic(&mut cli, &source);
        {
            let (root, store) = open_store(&cli, Some(&source)).unwrap();
            let client = client_for(&cli, &root, &store, &destination).unwrap();
            assert!(client.tags().await.unwrap().is_empty());
        }
        let before = server.received_requests().await.unwrap().len();
        cli.paperless_token_file = None;
        prepare_with(&mut cli, &[], Some(&source), |_, _, _, _| {
            Err(Error::Config("synthetic cancelled preparation"))
        })
        .unwrap();
        assert!(cli.prepared_destination_tokens.is_empty());
        assert!(
            cli.prepared_destination_failures
                .contains(&destination.name)
        );
        // Supplying a token after cancellation must not revive this operation.
        cli.paperless_token_file = Some(root.join("synthetic-token-must-not-be-read"));
        let (report, code) = modern::run(&cli).await.unwrap();
        assert_eq!(code, 1);
        assert!(
            report["destinations"][0]["error"]
                .as_str()
                .unwrap()
                .contains("preparation failed or was cancelled")
        );
        assert_eq!(server.received_requests().await.unwrap().len(), before);
        // An explicitly new preparation is a fresh reviewed action.
        prepare_synthetic(&mut cli, &source);
        assert!(cli.prepared_destination_failures.is_empty());
        let (root, store) = open_store(&cli, Some(&source)).unwrap();
        assert_eq!(
            token(&cli, &root, &store, &destination).unwrap().expose(),
            "synthetic-prepared-token"
        );
    }

    #[test]
    fn readding_a_removed_legacy_destination_restores_its_binding_and_rejects_rebinding() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let mut store = Store::open(&root, "synthetic-cli-restore-namespace").unwrap();
        let target = epost_cli::delivery::PaperlessTarget {
            schema_version: 1,
            url: "https://paperless.example.invalid/".into(),
            inbox_tag_id: 42,
            source_tag_id: Some(43),
            remember_token: false,
        };
        epost_cli::delivery::save_target(&mut store, &target).unwrap();
        let backup = Destination {
            name: "backup".into(),
            credential_binding: CredentialBinding::Named,
            kind: DestinationKind::Folder {
                path: root.join("copies"),
            },
        };
        destinations::save(&mut store, &backup).unwrap();
        destinations::remove(&mut store, "paperless").unwrap();
        assert_eq!(
            new_paperless_binding(&store, "paperless", &target.url).unwrap(),
            CredentialBinding::Legacy
        );
        assert!(
            new_paperless_binding(&store, "paperless", "https://another.example.invalid").is_err()
        );
        let restored = Destination {
            name: "paperless".into(),
            credential_binding: new_paperless_binding(&store, "paperless", &target.url).unwrap(),
            kind: DestinationKind::Paperless(target.into()),
        };
        destinations::save(&mut store, &restored).unwrap();
        assert_eq!(
            destinations::load(&store, "paperless")
                .unwrap()
                .credential_binding,
            CredentialBinding::Legacy
        );
        assert_eq!(
            new_paperless_binding(&store, "new", "https://another.example.invalid").unwrap(),
            CredentialBinding::Named
        );
    }
}
