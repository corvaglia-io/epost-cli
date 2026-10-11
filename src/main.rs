use clap::{Args, Parser, Subcommand};
use epost_cli::{
    api::{ApiClient, ClientOptions, Environment},
    config::{self, Login, LoginMethod, Profile, SyncScope as Scope},
    credentials,
    error::{Error, Result},
    keychain,
    models::{Letter, Mailbox, MailboxKind},
    pagination::{self, Inventory, Source},
    progress::{Event, Reporter},
    secret::Secret,
    storage::{SaveResult, Store},
};

mod interactive;
#[cfg(test)]
mod interactive_tests;
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    io::{self, IsTerminal, Read, Write},
    path::{Path, PathBuf},
};

#[derive(Parser)]
#[command(
    version,
    about = "Unofficial read-only ePost inbox and archive retrieval"
)]
struct Cli {
    /// Compact, structured JSON on stdout; diagnostics stay on stderr.
    #[arg(long, global = true)]
    json: bool,
    /// API environment (production unless a selected profile supplies it).
    #[arg(long, global = true, value_enum)]
    environment: Option<Environment>,
    /// Use saved mailbox, output, scope and login preferences.
    #[arg(long, global = true)]
    profile: Option<String>,
    /// Profile configuration file; otherwise use the platform configuration folder.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[arg(skip)]
    selected_profile: Option<Profile>,
    /// Show sync progress on stderr, including when stdout is JSON or redirected.
    #[arg(long, global = true, conflicts_with = "no_progress")]
    progress: bool,
    /// Suppress automatic terminal progress.
    #[arg(long, global = true, conflicts_with = "progress")]
    no_progress: bool,
    #[arg(long, global = true, value_enum)]
    tenant_kind: Option<MailboxKind>,
    #[arg(long, global = true)]
    tenant_id: Option<String>,
    #[arg(long, global = true, allow_hyphen_values = true)]
    company_id: Option<i64>,
    /// Read only the password from stdin instead of EPOST_PASSWORD or a prompt.
    #[arg(long, global = true)]
    password_stdin: bool,
    #[arg(long, global = true, default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..=120))]
    timeout_seconds: u64,
    #[arg(long, global = true, default_value_t = 2, value_parser = clap::value_parser!(u32).range(0..=5))]
    retries: u32,
    #[arg(long, global = true, default_value_t = 32, value_parser = clap::value_parser!(u32).range(1..=64))]
    max_response_mib: u32,
    /// Literal loopback origin used by the synthetic CLI integration tests.
    #[arg(long, global = true, hide = true)]
    mock_url: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Guided mailbox setup; remember the username and optionally use macOS Keychain.
    Setup {
        #[arg(long)]
        output: Option<PathBuf>,
        #[arg(long, value_enum)]
        scope: Option<Scope>,
    },
    /// Sign in once, browse readable inbox/archive pages, and download by number.
    Browse {
        #[arg(long)]
        output: Option<PathBuf>,
        #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u32).range(1..=1000))]
        page_size: u32,
    },
    /// Verify profile sign-in and remember its username; optionally save the password.
    Login {
        /// Explicitly save this profile's password in the local macOS Keychain.
        #[arg(long)]
        remember: bool,
    },
    /// Forget this profile's local Keychain password; retain its mailbox settings.
    Logout,
    /// Discover accessible tenants; never selects a mailbox automatically.
    Tenants,
    /// Retrieve inbox metadata.
    Inbox {
        #[command(subcommand)]
        command: InboxCommand,
    },
    /// Retrieve archive folders and metadata.
    Archive {
        #[command(subcommand)]
        command: ArchiveCommand,
    },
    /// Retrieve metadata for one letter ID in the selected mailbox.
    Show { letter_id: String },
    /// Download one inbox or archived letter by ID into the document store.
    Download {
        letter_id: String,
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Retrieve documents across an explicitly selected inbox/archive scope.
    Sync(SyncArgs),
}

#[derive(Subcommand)]
enum InboxCommand {
    List(ListArgs),
}

#[derive(Subcommand)]
enum ArchiveCommand {
    Folders,
    List {
        #[arg(long, conflicts_with = "directory_id")]
        root: bool,
        #[arg(long, required_unless_present = "root")]
        directory_id: Option<String>,
        #[command(flatten)]
        paging: ListArgs,
    },
}

#[derive(Args)]
struct ListArgs {
    #[arg(long, default_value_t = 48, value_parser = clap::value_parser!(u32).range(1..=1000))]
    page_size: u32,
    #[arg(long, default_value_t = 0, conflicts_with = "all", value_parser = clap::value_parser!(u32).range(0..=2147483647))]
    offset: u32,
    /// Walk pages from offset zero; report overlaps and unfinished traversal.
    #[arg(long)]
    all: bool,
    #[arg(long, default_value_t = 1000, value_parser = clap::value_parser!(u32).range(1..=100000))]
    max_pages: u32,
}

#[derive(Args)]
struct SyncArgs {
    #[arg(long, value_enum)]
    scope: Option<Scope>,
    #[arg(long)]
    output: Option<PathBuf>,
    #[arg(long, default_value_t = 48, value_parser = clap::value_parser!(u32).range(1..=1000))]
    page_size: u32,
    #[arg(long, default_value_t = 1000, value_parser = clap::value_parser!(u32).range(1..=100000))]
    max_pages: u32,
}

struct ResolvedSyncArgs {
    scope: Scope,
    output: PathBuf,
    page_size: u32,
    max_pages: u32,
}

fn environment(cli: &Cli) -> Environment {
    cli.selected_profile
        .as_ref()
        .map(|profile| profile.environment)
        .or(cli.environment)
        .unwrap_or(Environment::Production)
}

fn output(cli: &Cli, explicit: Option<&Path>) -> Result<PathBuf> {
    explicit
        .map(Path::to_path_buf)
        .or_else(|| cli.selected_profile.as_ref().map(|p| p.output.clone()))
        .ok_or(Error::Config(
            "supply --output or use a profile saved by setup",
        ))
}

fn load_profile(cli: &mut Cli) -> Result<()> {
    if matches!(cli.command, Command::Setup { .. }) {
        return Ok(());
    }
    if let Some(name) = &cli.profile {
        if cli.tenant_kind.is_some()
            || cli.tenant_id.is_some()
            || cli.company_id.is_some()
            || cli.environment.is_some()
        {
            return Err(Error::Config(
                "a profile supplies mailbox and environment; omit the explicit selection flags",
            ));
        }
        cli.selected_profile = Some(config::load_profile(
            &config::path(cli.config.as_deref())?,
            name,
        )?);
    }
    Ok(())
}

#[derive(Serialize)]
struct Retrieval {
    letter_id: Option<String>,
    source: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    document: Option<SaveResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Serialize)]
struct SyncReport {
    scope: Scope,
    /// Offset APIs provide no snapshot/total guarantee, even on a successful run.
    mailbox_coverage_verified: bool,
    pagination_complete: bool,
    inventories: Vec<Value>,
    documents: Vec<Retrieval>,
    issues: Vec<String>,
}

fn env_secret(name: &str) -> Result<Option<Secret>> {
    match std::env::var(name) {
        Ok(value) if value.is_empty() => {
            Err(Error::Config("credential environment variable is empty"))
        }
        Ok(value) => Ok(Some(Secret::new(value))),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(_) => Err(Error::Config(
            "credential environment variable is not valid Unicode",
        )),
    }
}

fn password(cli: &Cli) -> Result<(String, Secret)> {
    let username = match std::env::var("EPOST_USERNAME") {
        Ok(value) => value,
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err(Error::Config("account email is not valid Unicode"));
        }
        Err(_)
            if cli
                .selected_profile
                .as_ref()
                .and_then(|p| p.login.as_ref())
                .is_some() =>
        {
            cli.selected_profile
                .as_ref()
                .unwrap()
                .login
                .as_ref()
                .unwrap()
                .username
                .clone()
        }
        Err(_) if io::stdin().is_terminal() => {
            eprint!("ePost account email: ");
            io::stderr().flush()?;
            let mut value = String::new();
            io::stdin().read_line(&mut value)?;
            value.trim().to_owned()
        }
        _ => {
            return Err(Error::Config(
                "set EPOST_USERNAME for password authentication",
            ));
        }
    };
    if username.trim().is_empty() || username.len() > 4096 || username.chars().any(char::is_control)
    {
        return Err(Error::Config(
            "account email is empty, too long or contains control characters",
        ));
    }
    let secret = if cli.password_stdin {
        let mut input = String::new();
        io::stdin().lock().take(8193).read_to_string(&mut input)?;
        if input.len() > 8192 {
            return Err(Error::Config("password input is too long"));
        }
        let input = input.trim_end_matches(['\r', '\n']);
        if input.is_empty() || input.contains(['\r', '\n']) {
            return Err(Error::Config(
                "stdin must contain exactly one nonempty password line",
            ));
        }
        Secret::new(input.to_owned())
    } else if let Some(secret) = env_secret("EPOST_PASSWORD")? {
        secret
    } else if !matches!(cli.command, Command::Login { .. })
        && cli
            .selected_profile
            .as_ref()
            .and_then(|p| p.login.as_ref())
            .is_some_and(|login| login.method == LoginMethod::MacosKeychain)
    {
        native_keychain_allowed(cli)?;
        let profile = cli.selected_profile.as_ref().unwrap();
        if profile.login.as_ref().unwrap().username != username {
            return Err(Error::Config(
                "runtime username differs from saved sign-in; supply a password or run profile login again",
            ));
        }
        credentials::get(
            &keychain::NativeStore,
            &config::path(cli.config.as_deref())?,
            cli.profile.as_deref().unwrap(),
            environment(cli).base_url(),
            profile,
            &username,
        )?
    } else if io::stdin().is_terminal() {
        loop {
            let candidate = Secret::new(rpassword::prompt_password(
                "ePost/KLARA password (not SwissID): ",
            )?);
            if !candidate.expose().is_empty() && candidate.expose().len() <= 8192 {
                break candidate;
            }
            eprintln!("Password is empty or too long. Please try again.");
        }
    } else {
        return Err(Error::Config(
            "set EPOST_PASSWORD or use --password-stdin for unattended password authentication",
        ));
    };
    if secret.expose().is_empty() || secret.expose().len() > 8192 {
        return Err(Error::Config("password is empty or too long"));
    }
    Ok((username, secret))
}

fn native_keychain_allowed(cli: &Cli) -> Result<()> {
    if cli.mock_url.is_some() {
        return Err(Error::Config(
            "synthetic transports cannot access the native Keychain; supply synthetic runtime credentials",
        ));
    }
    if !keychain::supported() {
        return Err(Error::Config(
            "saved Keychain sign-in requires macOS; supply runtime credentials or use profile login without --remember",
        ));
    }
    Ok(())
}

fn uses_keychain(profile: &Profile) -> bool {
    profile
        .login
        .as_ref()
        .is_some_and(|login| login.method == LoginMethod::MacosKeychain)
}

fn persist_profile(
    cli: &Cli,
    name: &str,
    profile: &Profile,
    previous: Option<&Profile>,
    secret: Option<&Secret>,
) -> Result<()> {
    let path = config::path(cli.config.as_deref())?;
    // A copied macOS profile has no accessible local Keychain on another OS.
    // Explicit username-only login can replace that reference without claiming
    // that the password on the original Mac was removed.
    if !keychain::supported()
        && matches!(cli.command, Command::Login { remember: false })
        && !uses_keychain(profile)
    {
        return config::save_profile_if_unchanged(&path, name, profile, previous);
    }
    if uses_keychain(profile) || previous.is_some_and(uses_keychain) {
        native_keychain_allowed(cli)?;
        credentials::save_profile_login(
            &keychain::NativeStore,
            &path,
            name,
            profile,
            previous,
            secret,
        )
    } else {
        config::save_profile_if_unchanged(&path, name, profile, previous)
    }
}

fn progress_enabled(cli: &Cli) -> bool {
    !cli.no_progress && (cli.progress || (!cli.json && io::stderr().is_terminal()))
}

fn mailbox(cli: &Cli) -> Result<Mailbox> {
    if let Some(profile) = &cli.selected_profile {
        return Ok(profile.mailbox.clone());
    }
    let tenant_id = cli
        .tenant_id
        .clone()
        .filter(|id| !id.is_empty() && id.len() <= 4096)
        .ok_or(Error::Config("--tenant-id is required"))?;
    let company_id = cli.company_id.filter(|id| *id >= 0).ok_or(Error::Config(
        "--company-id must be explicitly supplied and nonnegative",
    ))?;
    let kind = cli
        .tenant_kind
        .ok_or(Error::Config("--tenant-kind private|business is required"))?;
    Ok(Mailbox {
        tenant_id,
        company_id,
        kind,
    })
}

fn client(cli: &Cli, mailbox: Option<Mailbox>) -> Result<ApiClient> {
    let options = ClientOptions {
        timeout_secs: cli.timeout_seconds,
        max_retries: cli.retries,
        max_response_bytes: cli.max_response_mib as usize * 1024 * 1024,
    };
    match cli.mock_url.as_deref() {
        Some(url) => ApiClient::for_test(url, mailbox, options),
        None => ApiClient::new(environment(cli), mailbox, options),
    }
}

async fn authenticate(cli: &Cli, client: &mut ApiClient, mailbox: &Mailbox) -> Result<()> {
    let token = env_secret("EPOST_BEARER_TOKEN")?;
    let key = env_secret("EPOST_API_KEY")?;
    if (token.is_some() && key.is_some())
        || ((token.is_some() || key.is_some()) && cli.password_stdin)
    {
        return Err(Error::Config("choose one authentication method"));
    }
    if let Some(token) = token {
        return client.set_bearer(token);
    }
    if let Some(key) = key {
        return client.set_api_key(key);
    }
    let (username, secret) = password(cli)?;
    let tenants = client.tenants_password(&username, &secret).await?;
    if !tenants
        .iter()
        .any(|t| t.tenant_id == mailbox.tenant_id && t.company_id == mailbox.company_id)
    {
        return Err(Error::Config(
            "selected tenant/company is not present in account tenant discovery",
        ));
    }
    client.login(&username, &secret).await
}

fn namespace(cli: &Cli, mailbox: &Mailbox) -> Result<String> {
    let origin = cli
        .mock_url
        .as_deref()
        .unwrap_or(environment(cli).base_url());
    serde_json::to_string(&(origin, &mailbox.tenant_id, mailbox.company_id, mailbox.kind))
        .map_err(|_| Error::Config("cannot construct mailbox namespace"))
}

fn value<T: Serialize>(data: T) -> Result<Value> {
    serde_json::to_value(data).map_err(|_| Error::InvalidResponse("output serialization failed"))
}

async fn list(client: &mut ApiClient, source: Source, args: &ListArgs) -> Result<(Value, u8)> {
    if args.all {
        let result = pagination::collect(client, source, args.page_size, args.max_pages).await?;
        let exit = if result.failure_exit_code == Some(3) {
            3
        } else if result.pagination_complete {
            0
        } else {
            2
        };
        return Ok((value(result)?, exit));
    }
    let letters = pagination::page(client, &source, args.page_size, args.offset).await?;
    Ok((
        json!({"source": source, "letters": letters, "offset": args.offset,
        "pagination_complete": false, "single_page": true}),
        0,
    ))
}

fn registered_pending(letter: &Letter) -> bool {
    if letter.registered_status.as_deref() == Some("PENDING") {
        return true;
    }
    letter.e_letter_registered == Some(true)
        && !matches!(
            letter.registered_status.as_deref(),
            Some("ACCEPTED" | "LEGALLY_ACCEPTED")
        )
}

async fn retrieve(
    client: &mut ApiClient,
    store: &mut Store,
    letter: &Letter,
    source: &str,
) -> Result<Retrieval> {
    let id = letter
        .id
        .as_deref()
        .filter(|id| !id.is_empty())
        .ok_or(Error::InvalidResponse("letter has no usable ID"))?;
    let existing = store.verify_existing(id, source)?;
    if registered_pending(letter) {
        return Ok(Retrieval {
            letter_id: Some(id.into()),
            source: source.into(),
            status: "awaiting_manual_acceptance_or_status_verification".into(),
            document: existing,
            error: None,
        });
    }
    if let Some(document) = existing {
        return Ok(Retrieval {
            letter_id: Some(id.into()),
            source: source.into(),
            status: document.status.clone(),
            document: Some(document),
            error: None,
        });
    }
    let content = client.content(id).await?;
    let document = store.save(id, source, &content.bytes, content.content_type.as_deref())?;
    Ok(Retrieval {
        letter_id: Some(id.into()),
        source: source.into(),
        status: document.status.clone(),
        document: Some(document),
        error: None,
    })
}

fn supported(record: &Retrieval) -> bool {
    matches!(record.status.as_str(), "saved_pdf" | "already_saved")
        && record
            .document
            .as_ref()
            .is_some_and(|d| d.media_kind == "pdf")
}

async fn sync(
    cli: &Cli,
    client: &mut ApiClient,
    mailbox: &Mailbox,
    args: &ResolvedSyncArgs,
) -> Result<(Value, u8)> {
    let mut reporter = Reporter::new(progress_enabled(cli));
    let result = sync_with_progress(cli, client, mailbox, args, &mut reporter).await;
    reporter.event(Event::Finished {
        exit_code: match &result {
            Ok((_, code)) => *code,
            Err(error) => error.exit_code(),
        },
    });
    if progress_enabled(cli)
        && let Ok((report, _)) = &result
    {
        for line in attention_lines(report) {
            let _ = writeln!(io::stderr().lock(), "{line}");
        }
    }
    result
}

async fn sync_with_progress(
    cli: &Cli,
    client: &mut ApiClient,
    mailbox: &Mailbox,
    args: &ResolvedSyncArgs,
    reporter: &mut Reporter,
) -> Result<(Value, u8)> {
    let mut store = Store::open(&args.output, &namespace(cli, mailbox)?)?;
    let mut report = SyncReport {
        scope: args.scope,
        mailbox_coverage_verified: false,
        pagination_complete: true,
        inventories: Vec::new(),
        documents: Vec::new(),
        issues: Vec::new(),
    };
    let mut sources = Vec::new();
    if matches!(args.scope, Scope::Inbox | Scope::Both) {
        sources.push(Source::Inbox);
    }
    if matches!(args.scope, Scope::Archive | Scope::Both) {
        sources.push(Source::ArchiveRoot);
        reporter.event(Event::ArchiveFolders);
        match client.directories().await {
            Ok(directories) => {
                let mut seen = std::collections::HashSet::new();
                for directory in directories.into_iter().take(10_001) {
                    if sources.len() > 10_000 {
                        report.pagination_complete = false;
                        report
                            .issues
                            .push("archive directory safety limit reached".into());
                        break;
                    }
                    match directory.directory_id.filter(|id| !id.is_empty()) {
                        Some(id) if seen.insert(id.clone()) => {
                            sources.push(Source::ArchiveDirectory(id))
                        }
                        Some(_) => {
                            report.pagination_complete = false;
                            report.issues.push("duplicate archive directory ID".into());
                        }
                        None => {
                            report.pagination_complete = false;
                            report
                                .issues
                                .push("archive directory has no documented usable selector".into());
                        }
                    }
                }
            }
            Err(error) => {
                report.pagination_complete = false;
                report
                    .issues
                    .push(format!("archive folder enumeration: {error}"));
                if error.exit_code() == 3 {
                    return Ok((value(report)?, 3));
                }
            }
        }
    }
    let mut exit = 0;
    for source in sources {
        reporter.event(Event::InventoryStarted {
            label: match source {
                Source::Inbox => "inbox",
                Source::ArchiveRoot => "archive root",
                Source::ArchiveDirectory(_) => "archive folder",
            }
            .into(),
        });
        if report.documents.len() >= 100_000 {
            report.pagination_complete = false;
            report
                .issues
                .push("sync document safety limit reached; remaining sources deferred".into());
            return Ok((value(report)?, exit.max(2)));
        }
        let inventory: Inventory = match pagination::collect_with_progress(
            client,
            source.clone(),
            args.page_size,
            args.max_pages,
            |event| {
                if let pagination::PageProgress::Started { page, documents } = event {
                    reporter.event(Event::Page { page, documents });
                }
            },
        )
        .await
        {
            Ok(inventory) => inventory,
            Err(error) => {
                report.pagination_complete = false;
                report.issues.push(format!("{}: {error}", source.label()));
                exit = exit.max(if error.exit_code() == 3 { 3 } else { 2 });
                if error.exit_code() == 3 {
                    break;
                }
                continue;
            }
        };
        report.pagination_complete &= inventory.pagination_complete;
        reporter.event(Event::InventoryComplete {
            documents: inventory.letters.len(),
            complete: inventory.pagination_complete,
        });
        report.inventories.push(json!({"source": inventory.source, "letters": inventory.letters.len(),
            "pages": inventory.pages, "pagination_complete": inventory.pagination_complete, "issues": inventory.issues,
            "failure_exit_code": inventory.failure_exit_code}));
        if inventory.failure_exit_code == Some(3) {
            for letter in inventory
                .letters
                .into_iter()
                .take(100_000 - report.documents.len())
            {
                reporter.event(Event::DocumentComplete {
                    status: "deferred_due_to_authentication_failure".into(),
                });
                report.documents.push(Retrieval {
                    letter_id: letter.id,
                    source: source.label(),
                    status: "deferred_due_to_authentication_failure".into(),
                    document: None,
                    error: None,
                });
            }
            report.issues.push(
                "authentication failed during pagination; remaining retrieval deferred".into(),
            );
            return Ok((value(report)?, exit.max(3)));
        }
        for (index, letter) in inventory.letters.iter().enumerate() {
            if report.documents.len() >= 100_000 {
                report.pagination_complete = false;
                report
                    .issues
                    .push("sync document safety limit reached; remaining letters deferred".into());
                return Ok((value(report)?, exit.max(2)));
            }
            reporter.event(Event::DocumentStarted {
                index: index + 1,
                total: inventory.letters.len(),
            });
            match retrieve(client, &mut store, letter, &source.label()).await {
                Ok(record) => {
                    reporter.event(Event::DocumentComplete {
                        status: record.status.clone(),
                    });
                    if !supported(&record) {
                        exit = exit.max(2);
                    }
                    report.documents.push(record);
                }
                Err(error) => {
                    reporter.event(Event::DocumentComplete {
                        status: "failed".into(),
                    });
                    let code = error.exit_code();
                    exit = exit.max(if code == 3 || code == 4 { code } else { 2 });
                    report.documents.push(Retrieval {
                        letter_id: letter.id.clone(),
                        source: source.label(),
                        status: "failed".into(),
                        document: None,
                        error: Some(error.to_string()),
                    });
                    if code == 3 {
                        report.issues.push(
                            "authentication failed during retrieval; remaining letters deferred"
                                .into(),
                        );
                        return Ok((value(report)?, exit));
                    }
                }
            }
        }
    }
    if !report.pagination_complete || !report.issues.is_empty() {
        exit = exit.max(2);
    }
    Ok((value(report)?, exit))
}

fn attention_lines(report: &Value) -> Vec<String> {
    let mut reasons = std::collections::BTreeMap::<String, usize>::new();
    let mut add = |reason: String| *reasons.entry(reason).or_default() += 1;
    if let Some(documents) = report["documents"].as_array() {
        for document in documents {
            match document["status"].as_str() {
                Some("saved_json" | "saved_binary" | "already_saved")
                    if document["document"]["media_kind"].as_str() != Some("pdf") =>
                {
                    add("unsupported content preserved locally; review its format manually".into())
                }
                Some("awaiting_manual_acceptance_or_status_verification") => {
                    add("check acceptance or status in ePost, then retry".into())
                }
                Some("failed") => add(summary_reason(document["error"].as_str().unwrap_or(""))),
                Some("deferred_due_to_authentication_failure") => {
                    add("sign-in failed; retrieval deferred until the next sync".into())
                }
                _ => {}
            }
        }
    }
    for issue in report["issues"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        add(summary_reason(issue));
    }
    for inventory in report["inventories"].as_array().into_iter().flatten() {
        for issue in inventory["issues"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            add(summary_reason(issue));
        }
    }
    let mut lines: Vec<_> = reasons
        .into_iter()
        .map(|(reason, count)| format!("Attention ({count}): {reason}."))
        .collect();
    if report["pagination_complete"] == false {
        lines.push("Inventory is incomplete. Check missing folders/documents in ePost; retrying may resolve temporary failures.".into());
    }
    lines
}

/// Only known reasons are rendered; provider IDs and arbitrary text stay out of progress.
fn summary_reason(reason: &str) -> String {
    if let Some(code) = reason
        .strip_prefix("provider returned HTTP ")
        .and_then(|s| s.parse::<u16>().ok())
    {
        return if code == 404 {
            "provider returned HTTP 404; check unavailable documents in ePost".into()
        } else {
            format!("provider returned HTTP {code}; retry or inspect the detailed report")
        };
    }
    match reason {
        "archive directory has no documented usable selector" => {
            "archive folder has no API selector; check its documents manually in ePost"
        }
        "duplicate archive directory ID" => "duplicate archive folder; inventory is uncertain",
        "duplicate IDs observed across pages" => {
            "documents overlapped across pages; inventory is uncertain"
        }
        "letter without usable ID" => "document has no usable ID; check it manually in ePost",
        "maximum page count reached" => "page limit reached; increase --max-pages and retry",
        "pagination made no progress" => {
            "pagination made no progress; retry and compare with ePost"
        }
        "network request failed or timed out" => "network request failed or timed out; retry",
        "authentication failed or expired" => "sign-in failed or expired; sign in and retry",
        _ if reason.contains("safety limit")
            || reason == "pagination offset exceeded API range" =>
        {
            "safety limit reached; inventory is incomplete"
        }
        _ if reason.contains("authentication failed during") => {
            "sign-in failed; some retrieval was deferred"
        }
        _ => "retrieval or inventory issue; inspect the detailed JSON report and ePost",
    }
    .into()
}

fn sync_summary(report: &Value, code: u8) -> Vec<String> {
    let documents = report["documents"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let count = |status: &str| documents.iter().filter(|d| d["status"] == status).count();
    let mut lines = vec![format!(
        "Sync: {} new PDFs, {} already saved, {} other formats, {} require action in ePost, {} failed.",
        count("saved_pdf"),
        count("already_saved"),
        count("saved_json") + count("saved_binary"),
        count("awaiting_manual_acceptance_or_status_verification"),
        count("failed")
    )];
    if code != 0 {
        lines.push(format!("Sync needs attention (exit {code})."));
    }
    lines.extend(attention_lines(report));
    lines.push("Compare with ePost; API pagination cannot prove complete mailbox coverage. Use --json for the detailed report.".into());
    lines
}

async fn run(cli: &Cli) -> Result<(Value, u8)> {
    if matches!(cli.command, Command::Logout) {
        let name = cli
            .profile
            .as_deref()
            .ok_or(Error::Config("logout requires --profile"))?;
        let previous = cli
            .selected_profile
            .as_ref()
            .ok_or(Error::Config("logout requires a saved profile"))?;
        let mut profile = previous.clone();
        if let Some(login) = profile.login.as_mut() {
            login.method = LoginMethod::Prompt;
        }
        persist_profile(cli, name, &profile, Some(previous), None)?;
        return Ok((
            json!({"login_method": "prompt", "password_forgotten": true}),
            0,
        ));
    }
    if matches!(cli.command, Command::Tenants) {
        let mut client = client(cli, None)?;
        let token = env_secret("EPOST_BEARER_TOKEN")?;
        if env_secret("EPOST_API_KEY")?.is_some() {
            return Err(Error::Config(
                "tenant discovery requires password or bearer authentication",
            ));
        }
        let tenants = if let Some(token) = token {
            if cli.password_stdin {
                return Err(Error::Config("choose one authentication method"));
            }
            client.tenants_bearer(&token).await?
        } else {
            let (username, secret) = password(cli)?;
            client.tenants_password(&username, &secret).await?
        };
        return Ok((value(tenants)?, 0));
    }
    let mailbox = mailbox(cli)?;
    let mut client = client(cli, Some(mailbox.clone()))?;
    let mut authentication_progress =
        Reporter::new(matches!(cli.command, Command::Sync(_)) && progress_enabled(cli));
    authentication_progress.event(Event::Authenticating);
    if let Err(error) = authenticate(cli, &mut client, &mailbox).await {
        authentication_progress.event(Event::Finished {
            exit_code: error.exit_code(),
        });
        return Err(error);
    }
    match &cli.command {
        Command::Setup { .. }
        | Command::Browse { .. }
        | Command::Login { .. }
        | Command::Logout => unreachable!(),
        Command::Tenants => unreachable!(),
        Command::Inbox {
            command: InboxCommand::List(args),
        } => list(&mut client, Source::Inbox, args).await,
        Command::Archive {
            command: ArchiveCommand::Folders,
        } => Ok((value(client.directories().await?)?, 0)),
        Command::Archive {
            command:
                ArchiveCommand::List {
                    root,
                    directory_id,
                    paging,
                },
        } => {
            let source = if *root {
                Source::ArchiveRoot
            } else {
                Source::ArchiveDirectory(
                    directory_id
                        .clone()
                        .ok_or(Error::Config("archive selector required"))?,
                )
            };
            list(&mut client, source, paging).await
        }
        Command::Show { letter_id } => Ok((value(client.letter(letter_id).await?)?, 0)),
        Command::Download { letter_id, output } => {
            let output = self::output(cli, output.as_deref())?;
            let mut store = Store::open(&output, &namespace(cli, &mailbox)?)?;
            let letter = client.letter(letter_id).await?;
            if letter.id.as_deref() != Some(letter_id) {
                return Err(Error::InvalidResponse(
                    "letter ID does not match requested ID",
                ));
            }
            let record = retrieve(&mut client, &mut store, &letter, "by-id").await?;
            let exit = if supported(&record) { 0 } else { 2 };
            Ok((value(record)?, exit))
        }
        Command::Sync(args) => {
            let args = ResolvedSyncArgs {
                scope: args
                    .scope
                    .or_else(|| cli.selected_profile.as_ref().map(|p| p.scope))
                    .ok_or(Error::Config(
                        "supply --scope or use a profile saved by setup",
                    ))?,
                output: output(cli, args.output.as_deref())?,
                page_size: args.page_size,
                max_pages: args.max_pages,
            };
            sync(cli, &mut client, &mailbox, &args).await
        }
    }
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let mut cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error)
            if matches!(
                error.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ) =>
        {
            return if error.print().is_ok() {
                0.into()
            } else {
                1.into()
            };
        }
        Err(_) => {
            // Clap diagnostics may echo unrecognized argument values. Never expose
            // them: a caller could have accidentally supplied a secret as a flag.
            let message = "invalid command line; use --help for usage";
            let diagnostic = if std::env::args_os().any(|arg| arg == "--json") {
                json!({"schema_version": 1, "status": "error", "exit_code": 2, "error": message})
                    .to_string()
            } else {
                message.into()
            };
            let _ = writeln!(io::stderr().lock(), "{diagnostic}");
            return 2.into();
        }
    };
    let missing = if cli.profile.is_none() {
        match &cli.command {
            Command::Login { .. } | Command::Logout => {
                Some("login and logout require --profile with a saved profile")
            }
            Command::Sync(args) if args.scope.is_none() || args.output.is_none() => {
                Some("sync requires --scope and --output, or a profile saved by setup")
            }
            Command::Download { output: None, .. } => {
                Some("download requires --output, or a profile saved by setup")
            }
            _ => None,
        }
    } else {
        None
    };
    if let Some(message) = missing {
        let diagnostic = if cli.json {
            json!({"schema_version": 1, "status": "error", "exit_code": 2, "error": message})
                .to_string()
        } else {
            message.into()
        };
        let _ = writeln!(io::stderr().lock(), "{diagnostic}");
        return 2.into();
    }
    if let Err(error) = load_profile(&mut cli) {
        return print_error(&cli, error);
    }
    if matches!(
        cli.command,
        Command::Setup { .. } | Command::Browse { .. } | Command::Login { .. }
    ) {
        let result = (|| {
            if cli.json || cli.password_stdin {
                return Err(Error::Config(
                    "setup, browse and login need an interactive terminal; omit --json and --password-stdin",
                ));
            }
            if env_secret("EPOST_BEARER_TOKEN")?.is_some() || env_secret("EPOST_API_KEY")?.is_some()
            {
                return Err(Error::Config(
                    "setup and browse use password sign-in; unset bearer/API-key variables or use the script commands",
                ));
            }
            interactive::Terminal::new()
        })();
        let mut terminal = match result {
            Ok(terminal) => terminal,
            Err(error) => return print_error(&cli, error),
        };
        return match interactive::run(&cli, &mut terminal).await {
            Ok(code) => code.into(),
            Err(error) => print_error(&cli, error),
        };
    }
    let (payload, code) = match run(&cli).await {
        Ok((data, code)) => (
            json!({"schema_version": 1, "status": if code == 0 { "ok" } else { "partial" }, "data": data}),
            code,
        ),
        Err(error) => {
            return print_error(&cli, error);
        }
    };
    if matches!(cli.command, Command::Sync(_)) && !cli.json && io::stdout().is_terminal() {
        for line in sync_summary(&payload["data"], code) {
            if writeln!(io::stdout().lock(), "{line}").is_err() {
                return 1.into();
            }
        }
        return code.into();
    }
    let rendered = if cli.json {
        serde_json::to_string(&payload)
    } else {
        serde_json::to_string_pretty(&payload)
    };
    match rendered {
        Ok(text) if writeln!(io::stdout().lock(), "{text}").is_ok() => code.into(),
        _ => 1.into(),
    }
}

fn print_error(cli: &Cli, error: Error) -> std::process::ExitCode {
    let code = error.exit_code();
    let diagnostic = if cli.json {
        json!({"schema_version": 1, "status": "error", "exit_code": code, "error": error.to_string()}).to_string()
    } else {
        error.to_string()
    };
    let _ = writeln!(io::stderr().lock(), "{diagnostic}");
    code.into()
}
