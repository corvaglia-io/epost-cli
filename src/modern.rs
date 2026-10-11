//! High-level routines: one command handles retrieval and every destination.
use super::*;
use epost_cli::{
    delivery::{self, DeliveryOptions, NamedDeliveryOptions},
    destinations::{self, DestinationKind},
    progress,
};

#[derive(Clone, Copy, clap::ValueEnum)]
pub(super) enum Location {
    Inbox,
    Archive,
    All,
}
impl Location {
    fn scope(self) -> Scope {
        match self {
            Self::Inbox => Scope::Inbox,
            Self::Archive => Scope::Archive,
            Self::All => Scope::Both,
        }
    }
}
#[derive(Args, Clone)]
pub(super) struct ScopeArgs {
    /// Mailbox area; defaults to inbox. Archive retrieves existing archived mail.
    #[arg(value_enum, default_value = "inbox")]
    pub target: Location,
    /// Restrict this run; repeat for multiple destinations. Default: every configured destination.
    #[arg(long = "destination")]
    pub destinations: Vec<String>,
    /// Fetch into local storage only.
    #[arg(long, conflicts_with = "destinations")]
    pub local: bool,
    /// Maximum new download attempts and submissions per destination in this run.
    #[arg(long, default_value_t=1000, value_parser=clap::value_parser!(u32).range(1..=100000))]
    pub limit: u32,
    #[arg(long)]
    pub output: Option<PathBuf>,
}
#[derive(Args, Clone)]
pub(super) struct StatusArgs {
    #[arg(value_enum, default_value = "inbox")]
    pub target: Location,
    #[arg(long)]
    pub output: Option<PathBuf>,
}
#[derive(Args, Clone)]
pub(super) struct DeliveryArgs {
    #[arg(long = "destination")]
    pub destinations: Vec<String>,
    #[arg(long, default_value_t=1000, value_parser=clap::value_parser!(u32).range(1..=100000))]
    pub limit: u32,
    #[arg(long)]
    pub output: Option<PathBuf>,
}
#[derive(Subcommand, Clone)]
pub(super) enum ProfileCommand {
    /// Show configured profiles and the default.
    List,
    /// Select the default profile for future bare commands.
    Use { name: String },
}

pub(super) async fn run(cli: &Cli) -> Result<(Value, u8)> {
    if !matches!(cli.command, Command::Profile { .. })
        && cli.selected_profile.is_none()
        && cli.tenant_id.is_none()
    {
        return Err(Error::Config(
            "no default profile; run epost profile list and epost profile use NAME, or epost setup",
        ));
    }
    match &cli.command {
        Command::Profile { command } => {
            let path = config::path(cli.config.as_deref())?;
            match command {
                ProfileCommand::Use { name } => {
                    config::set_default_profile(&path, name)?;
                    Ok((json!({"default_profile":name}), 0))
                }
                ProfileCommand::List => {
                    let default = config::default_profile(&path)?;
                    let profiles=config::list_profiles(&path)?.into_iter().map(|(name,p)|json!({"default":Some(&name)==default.as_ref(),"name":name,"kind":p.mailbox.kind})).collect::<Vec<_>>();
                    Ok((json!({"profiles":profiles}), 0))
                }
            }
        }
        Command::Deliver(args) | Command::Verify(args) => {
            deliveries(cli, args, matches!(cli.command, Command::Verify(_))).await
        }
        Command::Check(args) => {
            let mb = mailbox(cli)?;
            let root = output(cli, args.output.as_deref())?;
            // Validate selection before fetching mail; typos must not produce a partial run.
            let store = Store::open(&root, &namespace(cli, &mb)?)?;
            if !args.local {
                let _ = destinations::select(&store, &args.destinations)?;
            }
            drop(store);
            let mut api = client(cli, Some(mb.clone()))?;
            let mut reporter = Reporter::new(progress_enabled(cli));
            reporter.event(Event::Authenticating);
            let result = match authenticate(cli, &mut api, &mb).await {
                Ok(()) => {
                    sync(
                        cli,
                        &mut api,
                        &mb,
                        &ResolvedSyncArgs {
                            scope: args.target.scope(),
                            output: root.clone(),
                            page_size: 48,
                            max_pages: 1000,
                            max_documents: args.limit as usize,
                        },
                    )
                    .await
                }
                Err(error) => Err(error),
            };
            let (mut report, mut code) = match result {
                Ok((sync, code)) => (json!({"sync":sync,"sync_exit_code":code}), code),
                Err(error) => (
                    json!({"source_error":error.to_string(),"sync_exit_code":error.exit_code()}),
                    error.exit_code(),
                ),
            };
            // Integrity failures stop here. Source auth/network failures still allow pending delivery.
            if !args.local && code != 4 && !progress::stop_requested() {
                let options = DeliveryArgs {
                    destinations: args.destinations.clone(),
                    limit: args.limit,
                    output: Some(root),
                };
                match deliveries(cli, &options, false).await {
                    Ok((delivery, next)) => {
                        report["destinations"] = delivery["destinations"].clone();
                        code = code.max(next);
                    }
                    Err(error) => {
                        report["delivery_error"] = json!(error.to_string());
                        code = code.max(error.exit_code());
                    }
                }
            }
            report["local_only"] = json!(args.local);
            Ok((report, code))
        }
        Command::Status(args) => {
            let mb = mailbox(cli)?;
            let mut api = client(cli, Some(mb.clone()))?;
            let mut reporter = Reporter::new(progress_enabled(cli));
            reporter.event(Event::Authenticating);
            authenticate(cli, &mut api, &mb).await?;
            let root = output(cli, args.output.as_deref())?;
            let mut report = json!({});
            let mut code = 0;
            if matches!(args.target, Location::Inbox | Location::All) {
                let (inbox, next) = check_inbox(
                    cli,
                    &mut api,
                    &mb,
                    &CheckArgs {
                        unread_only: false,
                        output: Some(root.clone()),
                        sync_if_needed: false,
                        page_size: 48,
                        max_pages: 1000,
                    },
                )
                .await?;
                report["inbox"] = inbox;
                code = code.max(next);
            }
            if matches!(args.target, Location::Archive | Location::All) && code != 3 {
                match archive_status(&mut api).await {
                    Ok((archive, next)) => {
                        report["archive"] = archive;
                        code = code.max(next);
                    }
                    Err(error) => {
                        report["archive_error"] = json!(error.to_string());
                        code = code.max(error.exit_code());
                    }
                }
            }
            let (_, store) = destination_cli::open_store(cli, args.output.as_deref())?;
            report["destinations"] = local_status(&store)?;
            if report["destinations"]
                .as_array()
                .is_some_and(|rows| rows.iter().any(|r| r["error"].is_string()))
            {
                code = code.max(2);
            }
            Ok((report, code))
        }
        _ => Err(Error::Config("unsupported high-level command")),
    }
}

async fn archive_status(api: &mut ApiClient) -> Result<(Value, u8)> {
    let folders = api.directories().await?;
    if folders.len() > 10_000 {
        return Err(Error::InvalidResponse(
            "archive directory safety limit reached",
        ));
    }
    let root = pagination::collect(api, Source::ArchiveRoot, 48, 1000).await?;
    let mut incomplete = !root.pagination_complete;
    let mut code = root
        .failure_exit_code
        .unwrap_or(if incomplete { 2 } else { 0 });
    let mut count = root.letters.len();
    let mut seen = std::collections::BTreeSet::new();
    for folder in &folders {
        if code == 3 {
            break;
        }
        if progress::stop_requested() {
            incomplete = true;
            code = code.max(2);
            break;
        }
        match folder.directory_id.as_deref().filter(|s| !s.is_empty()) {
            Some(id) if seen.insert(id.to_owned()) => {
                match pagination::collect(api, Source::ArchiveDirectory(id.into()), 48, 1000).await
                {
                    Ok(part) => {
                        count += part.letters.len();
                        incomplete |= !part.pagination_complete;
                        code = code.max(
                            part.failure_exit_code
                                .unwrap_or(if part.pagination_complete { 0 } else { 2 }),
                        );
                    }
                    Err(error) => {
                        incomplete = true;
                        code = code.max(error.exit_code());
                    }
                }
            }
            None if folder.number_of_documents == Some(0)
                && folder.has_sub_directories == Some(false) => {}
            _ => {
                incomplete = true;
                code = code.max(2);
            }
        }
    }
    Ok((
        json!({"observed_documents":count,"folders":folders.len(),"pagination_complete":!incomplete,"mailbox_coverage_verified":false}),
        code,
    ))
}

pub(super) fn local_status(store: &Store) -> Result<Value> {
    let mut rows = Vec::new();
    for destination in destinations::list(store)? {
        match delivery::destination_status(store,&destination) {
            Ok(status)=>rows.push(json!({"name":destination.name,"kind":kind(&destination.kind),"state":status})),
            Err(error)=>rows.push(json!({"name":destination.name,"kind":kind(&destination.kind),"error":error.to_string()})),
        }
    }
    Ok(json!(rows))
}
fn kind(kind: &DestinationKind) -> &'static str {
    if matches!(kind, DestinationKind::Paperless(_)) {
        "paperless"
    } else {
        "folder"
    }
}

async fn deliveries(cli: &Cli, args: &DeliveryArgs, audit: bool) -> Result<(Value, u8)> {
    let (root, mut store) = destination_cli::open_store(cli, args.output.as_deref())?;
    let selected = destinations::select(&store, &args.destinations)?;
    let mut rows = Vec::new();
    let mut code = 0;
    for destination in selected {
        if progress::stop_requested() {
            rows.push(json!({"name":destination.name,"kind":kind(&destination.kind),"error":"stopped before this destination"}));
            code = code.max(2);
            continue;
        }
        let name = &destination.name;
        progress_line(
            cli,
            &format!(
                "Destination {name}: {}…",
                if audit {
                    "auditing saved copies"
                } else {
                    "resuming delivery"
                }
            ),
        );
        let options = DeliveryOptions {
            max_documents: args.limit as usize,
            poll_attempts: 31,
            poll_interval_ms: 2000,
        };
        let result = match &destination.kind {
            DestinationKind::Paperless(config) => {
                match destination_cli::client_for(cli, &root, &store, &destination) {
                    Ok(client) => {
                        match destination_cli::validate_paperless_catalogs(&client, config).await {
                            Ok(()) => {
                                if audit {
                                    delivery::verify_paperless(
                                        &mut store,
                                        &client,
                                        &destination,
                                        options,
                                        |e| paperless_cli::show_progress(cli, e),
                                    )
                                    .await
                                } else {
                                    delivery::deliver_paperless(
                                        &mut store,
                                        &client,
                                        &destination,
                                        NamedDeliveryOptions {
                                            delivery: options,
                                            audit: false,
                                        },
                                        |e| paperless_cli::show_progress(cli, e),
                                    )
                                    .await
                                }
                            }
                            Err(error) => Err(error),
                        }
                    }
                    Err(error) => Err(error),
                }
            }
            DestinationKind::Folder { .. } => {
                if audit {
                    delivery::verify_folder(&mut store, &destination, options, |e| {
                        paperless_cli::show_progress(cli, e)
                    })
                    .await
                } else {
                    delivery::deliver_folder(&mut store, &destination, options, |e| {
                        paperless_cli::show_progress(cli, e)
                    })
                    .await
                }
            }
        };
        match result {
            Ok(result) => {
                let next = if result.authentication_failed {
                    3
                } else if result.has_attention() {
                    2
                } else {
                    0
                };
                code = code.max(next);
                rows.push(json!({"name":name,"kind":kind(&destination.kind),"delivery":result,"exit_code":next}));
            }
            Err(error) => {
                code = code.max(error.exit_code());
                rows.push(json!({"name":name,"kind":kind(&destination.kind),"error":error.to_string(),"exit_code":error.exit_code()}));
            }
        }
    }
    Ok((json!({"destinations":rows,"audit":audit}), code))
}
pub(super) fn progress_line(cli: &Cli, line: &str) {
    if progress_enabled(cli) && !progress::publish_line(line) {
        eprintln!("{line}");
    }
}

pub(super) fn summary(report: &Value, code: u8) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(name) = report["default_profile"].as_str() {
        return vec![format!("Default profile: {name}.")];
    }
    if let Some(profiles) = report["profiles"].as_array() {
        return if profiles.is_empty() {
            vec!["No profiles yet. Run epost setup.".into()]
        } else {
            profiles
                .iter()
                .map(|p| {
                    format!(
                        "{}{} ({})",
                        p["name"].as_str().unwrap_or("?"),
                        if p["default"] == true {
                            " [default]"
                        } else {
                            ""
                        },
                        p["kind"].as_str().unwrap_or("?")
                    )
                })
                .collect()
        };
    }
    if report["sync"].is_object() {
        lines.extend(sync_summary(
            &report["sync"],
            report["sync_exit_code"].as_u64().unwrap_or(2) as u8,
        ));
    }
    if let Some(error) = report["source_error"].as_str() {
        lines.push(format!(
            "ePost retrieval: {error}. Saved documents remain available for delivery."
        ));
    }
    if let Some(error) = report["delivery_error"].as_str() {
        lines.push(format!("Delivery: {error}."));
    }
    if report["inbox"].is_object() {
        lines.extend(check_summary(&report["inbox"], code));
    }
    if let Some(error) = report["archive_error"].as_str() {
        lines.push(format!(
            "Archive: {error}. Earlier observations are retained."
        ));
    }
    if report["archive"].is_object() {
        lines.push(format!("Archive: {} documents observed across root and {} folders. Inventory {}. Coverage remains unverified.",report["archive"]["observed_documents"],report["archive"]["folders"],if report["archive"]["pagination_complete"]==true{"finished"}else{"incomplete"}));
    }
    if let Some(destinations) = report["destinations"].as_array() {
        if destinations.is_empty() {
            lines.push("No destinations configured. Downloads are retained locally; run epost destination add.".into());
        }
        for row in destinations {
            let name = row["name"].as_str().unwrap_or("?");
            if let Some(error) = row["error"].as_str() {
                lines.push(format!(
                    "{name}: {error}. Other destinations retain their own results."
                ));
                continue;
            }
            let d = &row["delivery"];
            if d.is_object() {
                let n = |key: &str| d[key].as_u64().unwrap_or(0);
                lines.push(format!("{name}: {} newly verified, {} verified again, {} previously confirmed, {} pending, {} uncertain, {} failed, {} unsupported, {} deferred, {} not attempted.",n("delivered"),n("already_delivered"),n("previously_confirmed"),n("pending"),n("uncertain"),n("failed"),n("unsupported"),n("deferred_limit"),n("not_attempted")));
                if let Some(items) = d["items"].as_array() {
                    let errors = items
                        .iter()
                        .filter_map(|i| i["error"].as_str())
                        .collect::<std::collections::BTreeSet<_>>();
                    for error in errors {
                        lines.push(format!("  Attention: {error}"));
                    }
                }
            } else {
                let s = &row["state"];
                lines.push(format!("{name}: {} previously verified, {} pending, {} uncertain, {} not submitted (local history only).",s["verified"],s["pending"],s["uncertain"],s["not_submitted"]));
            }
        }
    }
    if code != 0 {
        lines.push(format!("Needs attention (exit {code}). Run epost doctor for diagnostics; epost deliver resumes pending work."));
    }
    lines.push("ePost letters remain unchanged; local files are retained.".into());
    lines
}
