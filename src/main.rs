use clap::{Args, Parser, Subcommand, ValueEnum};
use epost_cli::{
    api::{ApiClient, ClientOptions, Environment},
    error::{Error, Result},
    models::{Letter, Mailbox, MailboxKind},
    pagination::{self, Inventory, Source},
    secret::Secret,
    storage::{SaveResult, Store},
};
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
    #[arg(long, global = true, value_enum, default_value = "production")]
    environment: Environment,
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
        output: PathBuf,
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

#[derive(Clone, Copy, ValueEnum, Serialize)]
#[serde(rename_all = "snake_case")]
enum Scope {
    Inbox,
    Archive,
    Both,
}

#[derive(Args)]
struct SyncArgs {
    #[arg(long, value_enum)]
    scope: Scope,
    #[arg(long)]
    output: PathBuf,
    #[arg(long, default_value_t = 48, value_parser = clap::value_parser!(u32).range(1..=1000))]
    page_size: u32,
    #[arg(long, default_value_t = 1000, value_parser = clap::value_parser!(u32).range(1..=100000))]
    max_pages: u32,
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
        Ok(value) if !value.is_empty() => value,
        _ if io::stdin().is_terminal() => {
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
    if username.is_empty() || username.len() > 4096 {
        return Err(Error::Config("account email is empty or too long"));
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
    } else if io::stdin().is_terminal() {
        Secret::new(rpassword::prompt_password(
            "ePost/KLARA password (not SwissID): ",
        )?)
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

fn mailbox(cli: &Cli) -> Result<Mailbox> {
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
        None => ApiClient::new(cli.environment, mailbox, options),
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
        .unwrap_or(cli.environment.base_url());
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
    args: &SyncArgs,
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
        if report.documents.len() >= 100_000 {
            report.pagination_complete = false;
            report
                .issues
                .push("sync document safety limit reached; remaining sources deferred".into());
            return Ok((value(report)?, exit.max(2)));
        }
        let inventory: Inventory =
            match pagination::collect(client, source.clone(), args.page_size, args.max_pages).await
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
        report.inventories.push(json!({"source": inventory.source, "letters": inventory.letters.len(),
            "pages": inventory.pages, "pagination_complete": inventory.pagination_complete, "issues": inventory.issues,
            "failure_exit_code": inventory.failure_exit_code}));
        if inventory.failure_exit_code == Some(3) {
            for letter in inventory
                .letters
                .into_iter()
                .take(100_000 - report.documents.len())
            {
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
        for letter in &inventory.letters {
            if report.documents.len() >= 100_000 {
                report.pagination_complete = false;
                report
                    .issues
                    .push("sync document safety limit reached; remaining letters deferred".into());
                return Ok((value(report)?, exit.max(2)));
            }
            match retrieve(client, &mut store, letter, &source.label()).await {
                Ok(record) => {
                    if !supported(&record) {
                        exit = exit.max(2);
                    }
                    report.documents.push(record);
                }
                Err(error) => {
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

async fn run(cli: &Cli) -> Result<(Value, u8)> {
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
    authenticate(cli, &mut client, &mailbox).await?;
    match &cli.command {
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
            let mut store = Store::open(Path::new(output), &namespace(cli, &mailbox)?)?;
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
        Command::Sync(args) => sync(cli, &mut client, &mailbox, args).await,
    }
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let cli = match Cli::try_parse() {
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
    let (payload, code) = match run(&cli).await {
        Ok((data, code)) => (
            json!({"schema_version": 1, "status": if code == 0 { "ok" } else { "partial" }, "data": data}),
            code,
        ),
        Err(error) => {
            let code = error.exit_code();
            let diagnostic = if cli.json {
                json!({"schema_version": 1, "status": "error", "exit_code": code, "error": error.to_string()}).to_string()
            } else {
                error.to_string()
            };
            let _ = writeln!(io::stderr().lock(), "{diagnostic}");
            return code.into();
        }
    };
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
