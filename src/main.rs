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
    summary::{attention_lines, sync_summary},
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
    /// Check unread or unsaved inbox items without downloading content.
    Check(CheckArgs),
}

#[derive(Args)]
struct CheckArgs {
    /// One unread-count request; does not check locally unsaved documents.
    #[arg(long)]
    unread_only: bool,
    /// Local document store for comparing IDs (otherwise use the profile).
    #[arg(long)]
    output: Option<PathBuf>,
    /// Sync the inbox if unread or locally unsaved items are observed.
    #[arg(long)]
    sync_if_needed: bool,
    #[arg(long, default_value_t = 48, value_parser = clap::value_parser!(u32).range(1..=1000))]
    page_size: u32,
    #[arg(long, default_value_t = 1000, value_parser = clap::value_parser!(u32).range(1..=100000))]
    max_pages: u32,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    diagnostic: Option<ContentDiagnostic>,
}

/// Fixed codes and flags only: never response bodies, titles or reference URLs.
#[derive(Serialize)]
struct ContentDiagnostic {
    reason_code: &'static str,
    content_http_status: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata_http_status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    letter_type: Option<&'static str>,
    has_message: bool,
    has_file_name: bool,
    has_content_reference: bool,
    registered_pending: bool,
}

#[derive(Serialize)]
struct SyncReport {
    scope: Scope,
    /// Offset APIs provide no snapshot/total guarantee, even on a successful run.
    mailbox_coverage_verified: bool,
    pagination_complete: bool,
    inventories: Vec<Value>,
    archive_directories: Vec<Value>,
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

fn content_diagnostic(letter: &Letter) -> ContentDiagnostic {
    let present = |field: &str| {
        letter
            .extra
            .get(field)
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty())
    };
    let letter_type = match letter.letter_type.as_deref() {
        Some("SIMPLE_SHORT_MESSAGE") => Some("SIMPLE_SHORT_MESSAGE"),
        Some("CLASSIC_LETTER") => Some("CLASSIC_LETTER"),
        Some("SMART_LETTER") => Some("SMART_LETTER"),
        Some("SMART_LETTER_ANSWER") => Some("SMART_LETTER_ANSWER"),
        Some("INCAMAIL") => Some("INCAMAIL"),
        Some("SECURESEND") => Some("SECURESEND"),
        _ => None,
    };
    ContentDiagnostic {
        reason_code: "content_unavailable_metadata_available",
        content_http_status: 404,
        metadata_http_status: None,
        metadata_error: None,
        letter_type,
        has_message: present("documentMessage"),
        has_file_name: letter
            .file_name
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty()),
        has_content_reference: present("letterContentReference"),
        registered_pending: registered_pending(letter),
    }
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
            diagnostic: None,
        });
    }
    if let Some(document) = existing {
        return Ok(Retrieval {
            letter_id: Some(id.into()),
            source: source.into(),
            status: document.status.clone(),
            document: Some(document),
            error: None,
            diagnostic: None,
        });
    }
    let content = match client.content(id).await {
        Ok(content) => content,
        Err(Error::Http(404)) => {
            // One canonical, bounded metadata check. Never follow a provider's
            // content reference or substitute metadata for a downloaded file.
            let mut diagnostic = content_diagnostic(letter);
            let mut status = "failed";
            match client.letter(id).await {
                Ok(current) if current.id.as_deref() == Some(id) => {
                    diagnostic = content_diagnostic(&current);
                    diagnostic.metadata_http_status = Some(200);
                    if registered_pending(&current) {
                        status = "awaiting_manual_acceptance_or_status_verification";
                    }
                }
                Ok(_) => {
                    diagnostic.reason_code = "metadata_id_mismatch";
                    diagnostic.metadata_http_status = Some(200);
                }
                Err(error) if error.exit_code() == 3 => return Err(error),
                Err(Error::Http(404)) => {
                    diagnostic.reason_code = "letter_unavailable";
                    diagnostic.metadata_http_status = Some(404);
                }
                Err(error) => {
                    diagnostic.reason_code = "metadata_check_failed";
                    if let Error::Http(code) = &error {
                        diagnostic.metadata_http_status = Some(*code);
                    }
                    diagnostic.metadata_error = Some(error.to_string());
                }
            }
            return Ok(Retrieval {
                letter_id: Some(id.into()),
                source: source.into(),
                status: status.into(),
                document: None,
                error: Some(Error::Http(404).to_string()),
                diagnostic: Some(diagnostic),
            });
        }
        Err(error) => return Err(error),
    };
    let document = store.save(id, source, &content.bytes, content.content_type.as_deref())?;
    Ok(Retrieval {
        letter_id: Some(id.into()),
        source: source.into(),
        status: document.status.clone(),
        document: Some(document),
        error: None,
        diagnostic: None,
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
    // TTY and guided views own the final human summary. JSON/pipe users can
    // opt into a single progress summary on stderr while stdout stays machine-readable.
    if matches!(cli.command, Command::Sync(_)) && (cli.json || !io::stdout().is_terminal()) {
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
        archive_directories: Vec::new(),
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
                for (index, directory) in directories.into_iter().enumerate() {
                    if index >= 10_000 {
                        report.pagination_complete = false;
                        report
                            .issues
                            .push("archive directory safety limit reached".into());
                        break;
                    }
                    let observation = json!({
                        "directory_id": directory.directory_id,
                        "name": directory.directory_name,
                        "number_of_documents": directory.number_of_documents,
                        "has_sub_directories": directory.has_sub_directories,
                        "parent_directory_ids": directory.parent_directory_ids,
                    });
                    let selection = match directory.directory_id.filter(|id| !id.is_empty()) {
                        Some(id) if seen.insert(id.clone()) => {
                            sources.push(Source::ArchiveDirectory(id));
                            "selected"
                        }
                        Some(_) => {
                            report.pagination_complete = false;
                            report.issues.push("duplicate archive directory ID".into());
                            "duplicate"
                        }
                        None if directory.number_of_documents == Some(0)
                            && directory.has_sub_directories == Some(false) =>
                        {
                            // An observation, not proof of complete mailbox coverage.
                            "unselectable_empty"
                        }
                        None => {
                            report.pagination_complete = false;
                            report
                                .issues
                                .push("archive directory has no documented usable selector".into());
                            "unselectable"
                        }
                    };
                    report.archive_directories.push(json!({
                        "directory": observation, "selection": selection,
                    }));
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
                    media_kind: None,
                });
                report.documents.push(Retrieval {
                    letter_id: letter.id,
                    source: source.label(),
                    status: "deferred_due_to_authentication_failure".into(),
                    document: None,
                    error: None,
                    diagnostic: None,
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
                        media_kind: record.document.as_ref().map(|d| d.media_kind.clone()),
                    });
                    if !supported(&record) {
                        exit = exit.max(2);
                    }
                    report.documents.push(record);
                }
                Err(error) => {
                    reporter.event(Event::DocumentComplete {
                        status: "failed".into(),
                        media_kind: None,
                    });
                    let code = error.exit_code();
                    exit = exit.max(if code == 3 || code == 4 { code } else { 2 });
                    report.documents.push(Retrieval {
                        letter_id: letter.id.clone(),
                        source: source.label(),
                        status: "failed".into(),
                        document: None,
                        error: Some(error.to_string()),
                        diagnostic: None,
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

async fn check_inbox(
    cli: &Cli,
    client: &mut ApiClient,
    mailbox: &Mailbox,
    args: &CheckArgs,
) -> Result<(Value, u8)> {
    let (mut report, mut code) = if args.unread_only {
        let count = client.inbox_unread_count().await?;
        (
            json!({
                "mode": "unread_count", "unread_count": count,
                "has_unread": count > 0, "has_new": null,
                "sync_needed": count > 0, "pagination_complete": null,
                "mailbox_coverage_verified": false, "issues": [],
            }),
            0,
        )
    } else {
        let destination = output(cli, args.output.as_deref())?;
        let store = Store::open(&destination, &namespace(cli, mailbox)?)?;
        let inventory =
            pagination::collect(client, Source::Inbox, args.page_size, args.max_pages).await?;
        let mut unread = 0;
        let mut unknown_read = 0;
        let mut unsaved = 0;
        let mut saved = 0;
        for letter in &inventory.letters {
            match letter.extra.get("readStatus").and_then(Value::as_str) {
                Some("UNREAD") => unread += 1,
                Some("READ") => {}
                _ => unknown_read += 1,
            }
            if let Some(id) = letter.id.as_deref().filter(|id| !id.is_empty()) {
                if store.contains_saved(id)? {
                    saved += 1;
                } else {
                    unsaved += 1;
                }
            }
        }
        let complete = inventory.pagination_complete;
        let has_new = if unsaved > 0 {
            Some(true)
        } else if complete {
            Some(false)
        } else {
            None
        };
        let has_unread = if unread > 0 {
            Some(true)
        } else if complete && unknown_read == 0 {
            Some(false)
        } else {
            None
        };
        let needed = if has_new == Some(true) || has_unread == Some(true) {
            Some(true)
        } else if has_new == Some(false) && has_unread == Some(false) {
            Some(false)
        } else {
            None
        };
        let code = if inventory.failure_exit_code == Some(3) {
            3
        } else if !complete || unknown_read > 0 {
            2
        } else {
            0
        };
        let mut issues = inventory.issues;
        if unknown_read > 0 {
            issues.push("some inbox items have missing or unknown read status".into());
        }
        (
            json!({
                "mode": "metadata_and_local_store", "inbox_documents": inventory.letters.len(),
                "unread_count": unread, "unknown_read_status_count": unknown_read,
                "new_documents_count": unsaved, "already_saved_count": saved,
                "has_unread": has_unread, "has_new": has_new, "sync_needed": needed,
                "pages": inventory.pages, "pagination_complete": complete,
                "mailbox_coverage_verified": false, "issues": issues,
            }),
            code,
        )
    };
    report["check_exit_code"] = json!(code);
    report["sync_requested"] = json!(args.sync_if_needed);
    report["sync_performed"] = json!(false);
    if args.sync_if_needed && report["sync_needed"] == true && code != 3 {
        // Store comparison guard is dropped before opening the sync store.
        let destination = output(cli, args.output.as_deref())?;
        let mut reporter = Reporter::new(progress_enabled(cli));
        let result = sync_with_progress(
            cli,
            client,
            mailbox,
            &ResolvedSyncArgs {
                scope: Scope::Inbox,
                output: destination,
                page_size: args.page_size,
                max_pages: args.max_pages,
            },
            &mut reporter,
        )
        .await;
        if cli.json || !io::stdout().is_terminal() {
            reporter.event(Event::Finished {
                exit_code: match &result {
                    Ok((_, code)) => *code,
                    Err(error) => error.exit_code(),
                },
            });
            if progress_enabled(cli)
                && let Ok((retrieval, _)) = &result
            {
                for line in attention_lines(retrieval) {
                    let _ = writeln!(io::stderr().lock(), "{line}");
                }
            }
        }
        code = attach_check_sync(&mut report, code, result);
    }
    Ok((report, code))
}

/// Preserve a completed check, including uncertainty, even if its requested
/// sync fails before producing a retrieval report.
fn attach_check_sync(report: &mut Value, check_code: u8, result: Result<(Value, u8)>) -> u8 {
    report["sync_performed"] = json!(true);
    let sync_code = match result {
        Ok((retrieval, code)) => {
            report["sync"] = retrieval;
            code
        }
        Err(error) => {
            report["sync_error"] = json!(error.to_string());
            error.exit_code()
        }
    };
    report["sync_exit_code"] = json!(sync_code);
    check_code.max(sync_code)
}

fn check_summary(report: &Value, code: u8) -> Vec<String> {
    let mut lines = if report["mode"] == "unread_count" {
        vec![format!(
            "Inbox: {} unread. Unsaved documents were not checked.",
            report["unread_count"]
        )]
    } else {
        vec![format!(
            "Inbox: {} unread, {} locally unsaved, {} already saved, {} with unknown read status.",
            report["unread_count"],
            report["new_documents_count"],
            report["already_saved_count"],
            report["unknown_read_status_count"]
        )]
    };
    if report["sync"].is_object() {
        lines.extend(sync_summary(
            &report["sync"],
            report["sync_exit_code"].as_u64().unwrap_or(2) as u8,
        ));
    } else {
        lines.push(
            match report["sync_needed"].as_bool() {
                Some(true) => "Inbox items need attention. Run sync to retrieve unsaved documents.",
                Some(false) if report["mode"] == "unread_count" => {
                    "No unread inbox items were observed. Unsaved documents were not checked."
                }
                Some(false) => "No unread or unsaved inbox items were observed.",
                None => "Inbox check is inconclusive; retry or inspect ePost.",
            }
            .into(),
        );
        lines.push(
            "Read status is not changed. Counts are observations; mailbox coverage is unverified."
                .into(),
        );
    }
    if code != 0
        && (report["sync"].is_null() || report["check_exit_code"].as_u64().unwrap_or(0) > 0)
    {
        lines.push(format!(
            "Check/sync needs attention (exit {code}); inspect the detailed report."
        ));
    }
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
        Command::Inbox {
            command: InboxCommand::Check(args),
        } => check_inbox(cli, &mut client, &mailbox, args).await,
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
            Command::Inbox {
                command: InboxCommand::Check(args),
            } if args.output.is_none() && (!args.unread_only || args.sync_if_needed) => {
                Some("inbox check needs --output or a profile, except standalone --unread-only")
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
    if matches!(
        cli.command,
        Command::Inbox {
            command: InboxCommand::Check(_)
        }
    ) && !cli.json
        && io::stdout().is_terminal()
    {
        for line in check_summary(&payload["data"], code) {
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

#[cfg(test)]
mod check_tests {
    use super::*;

    #[test]
    fn a_failed_conditional_sync_preserves_the_completed_partial_check() {
        let mut report = json!({"has_new":true,"pagination_complete":false,"check_exit_code":2});
        let exit = attach_check_sync(&mut report, 2, Err(Error::Network));
        assert_eq!(exit, 2);
        assert_eq!(report["has_new"], true);
        assert_eq!(report["pagination_complete"], false);
        assert_eq!(report["sync_exit_code"], 1);
        assert_eq!(report["sync_error"], "network request failed or timed out");
        assert!(
            check_summary(&report, exit)
                .iter()
                .any(|s| s.contains("Check/sync needs attention (exit 2)"))
        );
        let exit = attach_check_sync(
            &mut report,
            2,
            Err(Error::Storage("saved document is corrupt")),
        );
        assert_eq!(exit, 4);
    }

    #[test]
    fn successful_conditional_sync_does_not_hide_check_uncertainty() {
        let mut report = json!({"check_exit_code":2});
        let exit = attach_check_sync(
            &mut report,
            2,
            Ok((json!({"documents":[],"pagination_complete":true}), 0)),
        );
        assert_eq!(exit, 2);
        let lines = check_summary(&report, exit);
        assert!(
            lines
                .iter()
                .any(|s| s.contains("Check/sync needs attention (exit 2)"))
        );
        assert_eq!(report["sync_exit_code"], 0);
    }
}
