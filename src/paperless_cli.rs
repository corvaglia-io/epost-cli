//! Runtime-only Paperless setup and delivery. No ePost login is required here.
use super::*;
use epost_cli::{
    delivery::{self, DeliveryEvent, DeliveryOptions, DeliveryPhase, PaperlessTarget},
    keychain::CredentialStore,
    paperless::{PaperlessClient, PaperlessOptions, Tag, normalize_url},
};
use sha2::{Digest, Sha256};

#[derive(Subcommand)]
pub(super) enum PaperlessCommand {
    /// Verify a token, choose/create delivery tags, and save a secret-free target.
    Setup {
        /// Instance URL, including an optional installation path. Prompts if omitted.
        #[arg(long)]
        url: Option<String>,
        /// Existing tag with the Paperless inbox flag; otherwise use/create ePost Inbox.
        #[arg(long, value_parser = clap::value_parser!(i64).range(1..))]
        inbox_tag: Option<i64>,
        /// Existing source tag; otherwise use/create ePost.
        #[arg(long, value_parser = clap::value_parser!(i64).range(1..))]
        source_tag: Option<i64>,
        /// Explicitly save the token in the local, nonsynchronized macOS Keychain.
        #[arg(long)]
        remember: bool,
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Deliver saved PDFs, or resume pending deliveries, without signing in to ePost.
    Upload {
        #[arg(long)]
        output: Option<PathBuf>,
        #[arg(long, default_value_t = 1000, value_parser = clap::value_parser!(u32).range(1..=100000))]
        limit: u32,
        /// Wait between task checks for up to this many seconds; HTTP time is extra.
        #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u32).range(0..=600))]
        wait_seconds: u32,
    },
    /// Show local delivery state without credentials or network requests.
    Status {
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Adopt an existing Paperless document only after original-byte and inbox checks.
    Reconcile {
        letter_id: String,
        #[arg(long, value_parser = clap::value_parser!(i64).range(1..))]
        document_id: i64,
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Forget the saved local Paperless token; retain target and delivery history.
    Logout {
        #[arg(long)]
        output: Option<PathBuf>,
    },
}

fn open_store(cli: &Cli, explicit: Option<&Path>) -> Result<(PathBuf, Store)> {
    let root = output(cli, explicit)?;
    let store = Store::open(&root, &namespace(cli, &mailbox(cli)?)?)?;
    Ok((root, store))
}

fn make_client(cli: &Cli, url: &str, token: Secret) -> Result<PaperlessClient> {
    let options = PaperlessOptions {
        timeout_secs: cli.timeout_seconds,
        max_retries: cli.retries,
        max_response_bytes: cli.max_response_mib as usize * 1024 * 1024,
    };
    if cli.mock_url.is_some() {
        PaperlessClient::for_test(url, token, options)
    } else {
        PaperlessClient::new(url, token, options)
    }
}

const TOKEN_SERVICE: &str = "io.corvaglia.epost-cli.paperless";

fn token_account(root: &Path, store: &Store, url: &str) -> Result<String> {
    let identity = serde_json::to_vec(&(root, store.identity_hash(), url))
        .map_err(|_| Error::Config("cannot construct Paperless token identity"))?;
    Ok(hex::encode(Sha256::digest(identity)))
}

fn previous_target(store: &Store) -> Result<Option<PaperlessTarget>> {
    if store.read_private_state("paperless-target.json")?.is_some() {
        Ok(Some(delivery::load_target(store)?))
    } else {
        Ok(None)
    }
}

fn restore_tokens(
    backend: &impl CredentialStore,
    snapshot: Vec<(String, Option<Secret>)>,
) -> Result<()> {
    let mut failure = None;
    for (account, token) in snapshot {
        let result = match token {
            Some(token) => backend.set(TOKEN_SERVICE, &account, &token),
            None => backend.delete(TOKEN_SERVICE, &account),
        };
        if let Err(error) = result {
            failure.get_or_insert(error);
        }
    }
    failure.map_or(Ok(()), Err)
}

/// Token changes and configuration replacement share the store's writer lock.
/// Roll back only before configuration commit; after commit, retain its token.
fn persist_with_tokens(
    root: &Path,
    store: &mut Store,
    target: &PaperlessTarget,
    secret: Option<&Secret>,
    backend: &impl CredentialStore,
) -> Result<()> {
    let previous = previous_target(store)?;
    let mut changes = std::collections::BTreeMap::new();
    if let Some(previous) = previous.filter(|target| target.remember_token) {
        changes.insert(token_account(root, store, &previous.url)?, None);
    }
    if target.remember_token {
        changes.insert(
            token_account(root, store, &target.url)?,
            Some(
                secret
                    .ok_or(Error::Config(
                        "a token is required to remember Paperless sign-in",
                    ))?
                    .clone(),
            ),
        );
    }
    delivery::save_target_with_effect(
        store,
        target,
        || {
            let mut snapshot = Vec::new();
            for account in changes.keys() {
                snapshot.push((
                    account.clone(),
                    backend.get_optional(TOKEN_SERVICE, account)?,
                ));
            }
            for (account, token) in &changes {
                let result = match token {
                    Some(token) => backend.set(TOKEN_SERVICE, account, token),
                    None => backend.delete(TOKEN_SERVICE, account),
                };
                if let Err(error) = result {
                    restore_tokens(backend, snapshot).map_err(|_| Error::Storage(
                    "Paperless credential update and rollback failed; repair the credential store"
                ))?;
                    return Err(error);
                }
            }
            Ok(snapshot)
        },
        |snapshot| restore_tokens(backend, snapshot),
    )
}

fn file_token(path: &Path) -> Result<Secret> {
    use std::fs::OpenOptions;
    if !path.is_absolute() {
        return Err(Error::Config(
            "Paperless token file must have an absolute path",
        ));
    }
    let parent = path
        .parent()
        .ok_or(Error::Config("invalid token file path"))?;
    if parent.canonicalize()? != parent {
        return Err(Error::Config(
            "Paperless token file path must not contain symlink aliases",
        ));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let mut file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > 8192 {
        return Err(Error::Config(
            "Paperless token file is invalid or too large",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: geteuid has no arguments or memory effects.
        let uid = unsafe { libc::geteuid() };
        if metadata.uid() != uid || metadata.nlink() != 1 || metadata.mode() & 0o077 != 0 {
            return Err(Error::Config(
                "Paperless token file must be owned by you with private permissions and no hard links",
            ));
        }
    }
    let mut bytes = zeroize::Zeroizing::new(Vec::new());
    Read::by_ref(&mut file).take(8193).read_to_end(&mut bytes)?;
    if bytes.len() > 8192 {
        return Err(Error::Config("Paperless token file is too large"));
    }
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| Error::Config("Paperless token file must contain text"))?;
    checked_token(text.trim_end_matches(['\r', '\n']).to_owned())
}

fn checked_token(value: String) -> Result<Secret> {
    let secret = Secret::new(value);
    if secret.expose().is_empty()
        || secret.expose().len() > 8192
        || !secret.expose().is_ascii()
        || secret
            .expose()
            .bytes()
            .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
    {
        return Err(Error::Config("Paperless token is empty or invalid"));
    }
    Ok(secret)
}

fn token(cli: &Cli, root: &Path, store: &Store, target: &PaperlessTarget) -> Result<Secret> {
    if cli.password_stdin && matches!(cli.command, Command::Paperless { .. }) {
        return Err(Error::Config(
            "--password-stdin is for ePost; use --paperless-token-file or PAPERLESS_TOKEN",
        ));
    }
    if let Some(path) = &cli.paperless_token_file {
        if std::env::var_os("PAPERLESS_TOKEN").is_some() {
            return Err(Error::Config(
                "choose PAPERLESS_TOKEN or --paperless-token-file, not both",
            ));
        }
        return file_token(path);
    }
    if let Some(secret) = env_secret("PAPERLESS_TOKEN")? {
        return checked_token(secret.expose().to_owned());
    }
    if target.remember_token {
        native_keychain_allowed(cli)?;
        let secret = keychain::NativeStore.get(TOKEN_SERVICE, &token_account(root, store, &target.url)?)
            .map_err(|error| if keychain::is_missing(&error) {
                Error::Config("saved Paperless token was not found; run paperless setup --remember or supply runtime credentials")
            } else { error })?;
        return checked_token(secret.expose().to_owned());
    }
    if !cli.json && io::stdin().is_terminal() {
        eprintln!("Paperless token: My Profile → API token. This input is hidden.");
        return checked_token(rpassword::prompt_password("Paperless API token: ")?);
    }
    Err(Error::Config(
        "supply PAPERLESS_TOKEN, --paperless-token-file, or use paperless setup --remember on macOS",
    ))
}

async fn select_tag(
    client: &PaperlessClient,
    tags: &[Tag],
    explicit: Option<i64>,
    name: &str,
    inbox: bool,
) -> Result<Tag> {
    let selected = match explicit {
        Some(id) => client.tag(id).await?,
        None => {
            let matches: Vec<_> = tags.iter().filter(|tag| tag.name == name).collect();
            match matches.as_slice() {
                [] => client.create_tag(name, inbox).await?,
                [tag] => (*tag).clone(),
                _ => {
                    return Err(Error::Config(
                        "multiple Paperless tags match; specify --inbox-tag and --source-tag IDs",
                    ));
                }
            }
        }
    };
    if inbox && !selected.is_inbox_tag {
        return Err(Error::Config(
            "selected Paperless tag is not an inbox tag; enable its inbox flag or choose another --inbox-tag",
        ));
    }
    if !inbox && selected.is_inbox_tag {
        return Err(Error::Config(
            "Paperless source tag must not be an inbox tag; choose another --source-tag",
        ));
    }
    Ok(selected)
}

async fn validate_tags(client: &PaperlessClient, target: &PaperlessTarget) -> Result<()> {
    if !client.tag(target.inbox_tag_id).await?.is_inbox_tag {
        return Err(Error::Config(
            "configured Paperless inbox tag is missing its inbox flag; repair the tag or run paperless setup",
        ));
    }
    if let Some(id) = target.source_tag_id
        && client.tag(id).await?.is_inbox_tag
    {
        return Err(Error::Config(
            "configured Paperless source tag must not be an inbox tag; repair it or run paperless setup",
        ));
    }
    Ok(())
}

pub(super) async fn run(cli: &Cli, command: &PaperlessCommand) -> Result<(Value, u8)> {
    match command {
        PaperlessCommand::Setup {
            url,
            inbox_tag,
            source_tag,
            remember,
            output,
        } => {
            if *remember {
                native_keychain_allowed(cli)?;
            }
            let supplied = match url {
                Some(url) => url.clone(),
                None if !cli.json && io::stdin().is_terminal() => {
                    eprint!("Paperless instance URL (https://…): ");
                    io::stderr().flush()?;
                    let mut answer = String::new();
                    io::stdin().read_line(&mut answer)?;
                    answer.trim().to_owned()
                }
                None => {
                    return Err(Error::Config(
                        "supply paperless setup --url with your instance URL",
                    ));
                }
            };
            let url = normalize_url(&supplied, cli.mock_url.is_some())?;
            let (root, mut store) = open_store(cli, output.as_deref())?;
            if previous_target(&store)?.is_some_and(|target| target.remember_token) {
                native_keychain_allowed(cli)?;
            }
            // A new setup never silently borrows a saved token from another target.
            let mut target = PaperlessTarget {
                schema_version: 1,
                url,
                inbox_tag_id: 1,
                source_tag_id: None,
                remember_token: false,
            };
            // Reject rebinding an existing delivery ledger before remote tag writes.
            delivery::status(&store, &target)?;
            let secret = token(cli, &root, &store, &target)?;
            let client = make_client(cli, &target.url, secret.clone())?;
            let tags = client.tags().await?;
            let inbox = select_tag(&client, &tags, *inbox_tag, "ePost Inbox", true).await?;
            let source = select_tag(&client, &tags, *source_tag, "ePost", false).await?;
            target.inbox_tag_id = inbox.id;
            target.source_tag_id = Some(source.id);
            target.remember_token = *remember;
            persist_with_tokens(
                &root,
                &mut store,
                &target,
                Some(&secret),
                &keychain::NativeStore,
            )?;
            Ok((
                json!({"configured":true,"token_source":if *remember {"macos_keychain"} else {"runtime"},"inbox_tag_id":inbox.id,"source_tag_id":source.id,"epost_read_only":true}),
                0,
            ))
        }
        PaperlessCommand::Upload {
            output,
            limit,
            wait_seconds,
        } => deliver_saved(cli, output.as_deref(), *limit as usize, *wait_seconds).await,
        PaperlessCommand::Status { output } => {
            let (_, store) = open_store(cli, output.as_deref())?;
            let target = delivery::load_target(&store)?;
            Ok((value(delivery::status(&store, &target)?)?, 0))
        }
        PaperlessCommand::Reconcile {
            letter_id,
            document_id,
            output,
        } => {
            let (root, mut store) = open_store(cli, output.as_deref())?;
            let target = delivery::load_target(&store)?;
            let client = make_client(cli, &target.url, token(cli, &root, &store, &target)?)?;
            validate_tags(&client, &target).await?;
            let result =
                delivery::reconcile_document(&mut store, &client, &target, letter_id, *document_id)
                    .await?;
            Ok((json!({"reconciled":true,"delivery":value(result)?}), 0))
        }
        PaperlessCommand::Logout { output } => {
            let (root, mut store) = open_store(cli, output.as_deref())?;
            let mut target = delivery::load_target(&store)?;
            if target.remember_token {
                native_keychain_allowed(cli)?;
                target.remember_token = false;
                persist_with_tokens(&root, &mut store, &target, None, &keychain::NativeStore)?;
            }
            Ok((json!({"token_forgotten":true,"token_source":"runtime"}), 0))
        }
    }
}

pub(super) async fn deliver_saved(
    cli: &Cli,
    explicit: Option<&Path>,
    limit: usize,
    wait_seconds: u32,
) -> Result<(Value, u8)> {
    let (root, mut store) = open_store(cli, explicit)?;
    let target = delivery::load_target(&store)?;
    let client = make_client(cli, &target.url, token(cli, &root, &store, &target)?)?;
    validate_tags(&client, &target).await?;
    let options = DeliveryOptions {
        max_documents: limit,
        poll_attempts: wait_seconds / 2 + 1,
        poll_interval_ms: 2000,
    };
    let report = delivery::deliver(&mut store, &client, &target, options, |event| {
        show_progress(cli, event)
    })
    .await?;
    let code = if report.authentication_failed {
        3
    } else if report.has_attention() {
        2
    } else {
        0
    };
    Ok((value(report)?, code))
}

fn show_progress(cli: &Cli, event: DeliveryEvent) {
    if !progress_enabled(cli) {
        return;
    }
    let phase = match event.phase {
        DeliveryPhase::Started => "starting",
        DeliveryPhase::Uploading => "uploading PDF",
        DeliveryPhase::Waiting => "waiting for ingestion",
        DeliveryPhase::Verifying => "verifying original",
        DeliveryPhase::Reconciling => "checking earlier upload",
        DeliveryPhase::Finished => "finished",
    };
    let _ = writeln!(
        io::stderr().lock(),
        "Paperless: {phase} ({}/{}, {} verified, {} need attention)",
        event.processed,
        event.total,
        event.delivered,
        event.attention
    );
}

pub(super) fn summary(report: &Value, code: u8) -> Vec<String> {
    if report["configured"] == true {
        return vec![format!("Paperless configured. Token source: {}.", report["token_source"].as_str().unwrap_or("runtime")), "New PDFs will carry ePost Inbox and ePost tags. Review them in Paperless and remove the inbox tag when handled.".into(), "Run paperless upload for saved PDFs, or sync --paperless for retrieval and delivery. ePost remains read-only.".into()];
    }
    if report["token_forgotten"] == true {
        return vec!["Saved Paperless token forgotten; delivery history is retained.".into()];
    }
    if report["reconciled"] == true {
        return vec!["Paperless document verified and adopted. ePost remains read-only.".into()];
    }
    if report.get("delivered").is_none() {
        return vec![
            "Local Paperless state (no server verification performed):".into(),
            serde_json::to_string_pretty(report).unwrap_or_default(),
        ];
    }
    let count = |name| report[name].as_u64().unwrap_or(0);
    let mut lines = vec![format!(
        "Paperless: {} newly verified, {} already delivered, {} pending, {} uncertain, {} failed, {} unsupported, {} deferred, {} not attempted.",
        count("delivered"),
        count("already_delivered"),
        count("pending"),
        count("uncertain"),
        count("failed"),
        count("unsupported"),
        count("deferred_limit"),
        count("not_attempted")
    )];
    if let Some(items) = report["items"].as_array() {
        let mut errors = std::collections::BTreeSet::new();
        for item in items {
            if let Some(error) = item["error"].as_str() {
                errors.insert(error);
            }
        }
        lines.extend(
            errors
                .into_iter()
                .map(|error| format!("Attention: {error}")),
        );
    }
    if code != 0 {
        lines.push("Delivery needs attention. Retry paperless upload to resume tasks; uncertain uploads are checked without blindly uploading again. Use --json for details.".into());
    }
    lines
}

pub(super) fn sync_lines(report: &Value) -> Vec<String> {
    if let Some(error) = report["paperless_error"].as_str() {
        vec![format!(
            "Paperless delivery failed: {error}. Local downloads are retained."
        )]
    } else if !report["paperless"].is_null() {
        summary(
            &report["paperless"],
            report["paperless_exit_code"].as_u64().unwrap_or(2) as u8,
        )
    } else {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        cell::{Cell, RefCell},
        collections::BTreeMap,
    };

    #[derive(Default)]
    struct FakeTokens {
        entries: RefCell<BTreeMap<String, Secret>>,
        fail_next_set: Cell<bool>,
    }
    impl CredentialStore for FakeTokens {
        fn get(&self, _: &str, account: &str) -> Result<Secret> {
            self.entries
                .borrow()
                .get(account)
                .cloned()
                .ok_or(Error::Config(keychain::MISSING_ENTRY_MESSAGE))
        }
        fn set(&self, _: &str, account: &str, token: &Secret) -> Result<()> {
            if self.fail_next_set.replace(false) {
                return Err(Error::Config("synthetic token-store failure"));
            }
            self.entries
                .borrow_mut()
                .insert(account.to_owned(), token.clone());
            Ok(())
        }
        fn delete(&self, _: &str, account: &str) -> Result<()> {
            self.entries.borrow_mut().remove(account);
            Ok(())
        }
    }
    fn target() -> PaperlessTarget {
        PaperlessTarget {
            schema_version: 1,
            url: "https://paperless.example.invalid/".into(),
            inbox_tag_id: 1,
            source_tag_id: Some(2),
            remember_token: true,
        }
    }
    fn root() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("synthetic-paperless-tokens-")
            .tempdir_in(std::env::temp_dir().canonicalize().unwrap())
            .unwrap()
    }

    #[test]
    fn remember_to_runtime_retires_the_saved_token_and_logout_keeps_history() {
        let root = root();
        let mut store = Store::open(root.path(), "synthetic-mailbox").unwrap();
        let tokens = FakeTokens::default();
        let mut target = target();
        persist_with_tokens(
            root.path(),
            &mut store,
            &target,
            Some(&Secret::new("synthetic-token".into())),
            &tokens,
        )
        .unwrap();
        assert_eq!(tokens.entries.borrow().len(), 1);
        target.remember_token = false;
        persist_with_tokens(root.path(), &mut store, &target, None, &tokens).unwrap();
        assert!(tokens.entries.borrow().is_empty());
        assert!(!delivery::load_target(&store).unwrap().remember_token);
    }

    #[test]
    fn replacing_target_retires_old_identity_and_failure_restores_it() {
        let root = root();
        let mut store = Store::open(root.path(), "synthetic-mailbox").unwrap();
        let tokens = FakeTokens::default();
        let old = target();
        persist_with_tokens(
            root.path(),
            &mut store,
            &old,
            Some(&Secret::new("synthetic-original".into())),
            &tokens,
        )
        .unwrap();
        let mut new = old.clone();
        new.url = "https://other.example.invalid/".into();
        tokens.fail_next_set.set(true);
        assert!(
            persist_with_tokens(
                root.path(),
                &mut store,
                &new,
                Some(&Secret::new("synthetic-replacement".into())),
                &tokens
            )
            .is_err()
        );
        assert_eq!(delivery::load_target(&store).unwrap(), old);
        assert_eq!(tokens.entries.borrow().len(), 1);
        assert_eq!(
            tokens
                .get(
                    TOKEN_SERVICE,
                    &token_account(root.path(), &store, &old.url).unwrap()
                )
                .unwrap()
                .expose(),
            "synthetic-original"
        );
        persist_with_tokens(
            root.path(),
            &mut store,
            &new,
            Some(&Secret::new("synthetic-replacement".into())),
            &tokens,
        )
        .unwrap();
        assert_eq!(tokens.entries.borrow().len(), 1);
        assert!(
            tokens
                .get(
                    TOKEN_SERVICE,
                    &token_account(root.path(), &store, &old.url).unwrap()
                )
                .is_err()
        );
    }

    #[test]
    fn token_identity_binds_store_path_mailbox_and_destination() {
        let first = root();
        let second = root();
        let store = Store::open(first.path(), "synthetic-one").unwrap();
        let other = Store::open(second.path(), "synthetic-two").unwrap();
        let url = target().url;
        assert_ne!(
            token_account(first.path(), &store, &url).unwrap(),
            token_account(second.path(), &store, &url).unwrap()
        );
        assert_ne!(
            token_account(first.path(), &store, &url).unwrap(),
            token_account(first.path(), &other, &url).unwrap()
        );
        assert_ne!(
            token_account(first.path(), &store, &url).unwrap(),
            token_account(first.path(), &store, "https://other.example.invalid/").unwrap()
        );
    }
}
