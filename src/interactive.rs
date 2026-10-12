//! Guided terminal UI. Request/storage behavior is shared with script commands.
use super::*;
use std::io::BufRead;

pub(crate) trait Prompts {
    /// None means cancellation/EOF. Invalid answers are retried by the caller.
    fn input(&mut self, prompt: &str) -> Result<Option<String>>;
    fn credentials(&mut self, cli: &Cli) -> Result<(String, Secret)>;
    fn line(&mut self, text: &str) -> Result<()>;
    fn supports_keychain(&self) -> bool {
        false
    }
    fn persist_profile(
        &mut self,
        cli: &Cli,
        name: &str,
        profile: &Profile,
        previous: Option<&Profile>,
        _secret: Option<&Secret>,
    ) -> Result<()> {
        if uses_keychain(profile) || previous.is_some_and(uses_keychain) {
            return Err(Error::Config(
                "this frontend does not support password storage",
            ));
        }
        config::save_profile_if_unchanged(
            &config::path(cli.config.as_deref())?,
            name,
            profile,
            previous,
        )
    }
}

pub(crate) struct Terminal;

impl Terminal {
    pub(crate) fn new() -> Result<Self> {
        if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
            return Err(Error::Config(
                "open a terminal for setup/browse; use inbox, archive or sync for automation",
            ));
        }
        Ok(Self)
    }
}

impl Prompts for Terminal {
    fn input(&mut self, prompt: &str) -> Result<Option<String>> {
        loop {
            eprint!("{prompt}");
            io::stderr().flush()?;
            let mut answer = String::new();
            let stdin = io::stdin();
            let mut input = stdin.lock();
            let read = input.by_ref().take(4097).read_line(&mut answer)?;
            if read == 0 {
                return Ok(None);
            }
            if answer.len() > 4096 {
                if !answer.ends_with('\n') {
                    input.skip_until(b'\n')?;
                }
                self.line("That answer is too long. Please try again.")?;
                continue;
            }
            let answer = answer.trim().to_owned();
            return Ok(if answer.eq_ignore_ascii_case("q") {
                None
            } else {
                Some(answer)
            });
        }
    }

    fn credentials(&mut self, cli: &Cli) -> Result<(String, Secret)> {
        password(cli)
    }

    fn line(&mut self, text: &str) -> Result<()> {
        writeln!(io::stderr().lock(), "{text}")?;
        Ok(())
    }

    fn supports_keychain(&self) -> bool {
        keychain::supported()
    }
    fn persist_profile(
        &mut self,
        cli: &Cli,
        name: &str,
        profile: &Profile,
        previous: Option<&Profile>,
        secret: Option<&Secret>,
    ) -> Result<()> {
        persist_profile(cli, name, profile, previous, secret)
    }
}

/// Provider strings must never control the user's terminal. Raw IDs stay separate.
pub(crate) fn safe_line(text: &str) -> String {
    let mut clean = safe_text(&text.chars().take(120).collect::<String>());
    if text.chars().nth(120).is_some() {
        clean.push('…');
    }
    clean
}

fn safe_text(text: &str) -> String {
    text.chars().map(|c| {
        if c.is_control() || matches!(c, '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}') { ' ' } else { c }
    }).collect()
}

fn cancelled(answer: Option<String>) -> Option<String> {
    answer.filter(|answer| !answer.eq_ignore_ascii_case("q"))
}

fn number(answer: &str, count: usize) -> Option<usize> {
    answer
        .trim()
        .parse::<usize>()
        .ok()
        .filter(|n| *n >= 1 && *n <= count)
}

fn kind(prompts: &mut impl Prompts) -> Result<Option<MailboxKind>> {
    prompts.line("Mailbox kind: 1 = private, 2 = business. Type q to cancel.")?;
    loop {
        let Some(answer) = cancelled(prompts.input("Mailbox kind: ")?) else {
            return Ok(None);
        };
        match answer.to_ascii_lowercase().as_str() {
            "1" | "private" => return Ok(Some(MailboxKind::Private)),
            "2" | "business" => return Ok(Some(MailboxKind::Business)),
            _ => prompts.line("Choose 1 or 2, or type private or business. Please try again.")?,
        }
    }
}

fn scope(prompts: &mut impl Prompts) -> Result<Option<Scope>> {
    prompts.line("Sync scope: 1 = inbox, 2 = archive, 3 = both.")?;
    loop {
        let Some(answer) = cancelled(prompts.input("Sync scope: ")?) else {
            return Ok(None);
        };
        match answer.to_ascii_lowercase().as_str() {
            "1" | "inbox" => return Ok(Some(Scope::Inbox)),
            "2" | "archive" => return Ok(Some(Scope::Archive)),
            "3" | "both" => return Ok(Some(Scope::Both)),
            _ => prompts.line("Choose 1, 2 or 3. Please try again.")?,
        }
    }
}

fn destination(
    cli: &Cli,
    explicit: Option<&Path>,
    prompts: &mut impl Prompts,
) -> Result<Option<PathBuf>> {
    if explicit.is_some() || cli.selected_profile.is_some() {
        return output(cli, explicit).map(Some);
    }
    prompts.line("Choose a private folder for documents. Use an absolute physical path.")?;
    let default = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(|home| PathBuf::from(home).join("Downloads/epost-cli-data"));
    let prompt = default
        .as_ref()
        .map(|path| format!("Document folder [{}]: ", safe_text(&path.to_string_lossy())))
        .unwrap_or_else(|| "Document folder: ".into());
    loop {
        let Some(answer) = cancelled(prompts.input(&prompt)?) else {
            return Ok(None);
        };
        let path = if answer.is_empty() {
            match &default {
                Some(path) => path.clone(),
                None => PathBuf::new(),
            }
        } else {
            PathBuf::from(answer)
        };
        if valid_destination(&path) {
            return Ok(Some(path));
        }
        prompts.line("Use an absolute folder path without symlinks or '..'. Please try again.")?;
    }
}

fn valid_destination(path: &Path) -> bool {
    if !path.is_absolute()
        || path.file_name().is_none()
        || path
            .as_os_str()
            .to_string_lossy()
            .chars()
            .any(char::is_control)
        || path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return false;
    }
    path.ancestors()
        .all(|ancestor| match std::fs::symlink_metadata(ancestor) {
            Ok(metadata) => metadata.is_dir() && !metadata.file_type().is_symlink(),
            Err(error) => error.kind() == io::ErrorKind::NotFound,
        })
}

async fn sign_in(
    cli: &Cli,
    prompts: &mut impl Prompts,
) -> Result<Option<(ApiClient, Mailbox, String, Secret)>> {
    if cli.tenant_kind.is_some() || cli.tenant_id.is_some() || cli.company_id.is_some() {
        return Err(Error::Config(
            "guided commands select the mailbox; use a profile or the numbered choices",
        ));
    }
    prompts.line(
        "Sign in with your ePost/KLARA password. It is different from your SwissID password.",
    )?;
    let (username, secret) = prompts.credentials(cli)?;
    let mut discovery = client(cli, None)?;
    let tenants = discovery.tenants_password(&username, &secret).await?;
    if tenants.is_empty() {
        if cli.selected_profile.is_some() {
            return Err(Error::Config(
                "profile mailbox is not in this account; run setup again",
            ));
        }
        prompts.line("No accessible mailboxes were returned for this account.")?;
        return Ok(None);
    }
    let mailbox = if let Some(profile) = &cli.selected_profile {
        if !tenants.iter().any(|tenant| {
            tenant.tenant_id == profile.mailbox.tenant_id
                && tenant.company_id == profile.mailbox.company_id
        }) {
            return Err(Error::Config(
                "profile mailbox is not in this account; run setup again",
            ));
        }
        prompts.line(&format!(
            "Using profile {}.",
            safe_line(cli.profile.as_deref().unwrap_or(""))
        ))?;
        profile.mailbox.clone()
    } else {
        let Some(kind) = kind(prompts)? else {
            return Ok(None);
        };
        prompts.line("Choose your mailbox explicitly, even if only one is listed:")?;
        for (index, tenant) in tenants.iter().enumerate() {
            prompts.line(&format!(
                "  {}. {}  (company {}; tenant {})",
                index + 1,
                safe_line(tenant.company_name.as_deref().unwrap_or("Unnamed mailbox")),
                tenant.company_id,
                safe_line(&tenant.tenant_id)
            ))?;
        }
        let selected = loop {
            let Some(answer) = cancelled(prompts.input("Mailbox number: ")?) else {
                return Ok(None);
            };
            if let Some(number) = number(&answer, tenants.len()) {
                break &tenants[number - 1];
            }
            prompts.line("Choose a mailbox number from the list. Please try again.")?;
        };
        Mailbox {
            tenant_id: selected.tenant_id.clone(),
            company_id: selected.company_id,
            kind,
        }
    };
    let mut selected = client(cli, Some(mailbox.clone()))?;
    selected.login(&username, &secret).await?;
    prompts.line("Signed in. The session ends when you quit.")?;
    Ok(Some((selected, mailbox, username, secret)))
}

pub(crate) async fn run(cli: &Cli, prompts: &mut impl Prompts) -> Result<u8> {
    match &cli.command {
        Command::Setup {
            output: Some(path), ..
        }
        | Command::Browse {
            output: Some(path), ..
        } if !valid_destination(path) => {
            return Err(Error::Config(
                "document folder must be absolute and have no symlink/parent components",
            ));
        }
        _ => {}
    }
    if matches!(cli.command, Command::Setup { .. }) {
        // Catch invalid names, unsafe paths and corrupt config before sign-in.
        let path = config::path(cli.config.as_deref())?;
        config::profile_exists(&path, cli.profile.as_deref().unwrap_or("personal"))?;
    }
    if matches!(cli.command, Command::Login { remember: true }) && !prompts.supports_keychain() {
        return Err(Error::Config(
            "--remember requires macOS Keychain; omit it to remember only the username",
        ));
    }
    let Some((mut client, mailbox, username, secret)) = sign_in(cli, prompts).await? else {
        return Ok(0);
    };
    match &cli.command {
        Command::Setup {
            output: explicit,
            scope: explicit_scope,
        } => {
            let Some(output) = destination(cli, explicit.as_deref(), prompts)? else {
                return Ok(0);
            };
            if !valid_destination(&output) {
                return Err(Error::Config(
                    "document folder must be absolute and have no symlink/parent components",
                ));
            }
            let selected_scope = match explicit_scope {
                Some(scope) => *scope,
                None => match scope(prompts)? {
                    Some(scope) => scope,
                    None => return Ok(0),
                },
            };
            let name = cli.profile.as_deref().unwrap_or("personal");
            let path = config::path(cli.config.as_deref())?;
            let previous = if config::profile_exists(&path, name)? {
                Some(config::load_profile(&path, name)?)
            } else {
                None
            };
            if previous.is_some() {
                loop {
                    let Some(answer) =
                        cancelled(prompts.input("This profile exists. Replace it? (yes/no): ")?)
                    else {
                        return Ok(0);
                    };
                    match answer.to_ascii_lowercase().as_str() {
                        "yes" | "y" => break,
                        "no" | "n" => {
                            prompts.line("Profile kept.")?;
                            return Ok(0);
                        }
                        _ => prompts.line("Type yes or no. Please try again.")?,
                    }
                }
            }
            let mut profile = Profile {
                mailbox,
                environment: environment(cli),
                output,
                scope: selected_scope,
                login: Some(Login {
                    username: username.clone(),
                    method: LoginMethod::Prompt,
                }),
            };
            prompts.persist_profile(cli, name, &profile, previous.as_ref(), None)?;
            prompts.line("Saved: mailbox, document folder, sync scope and account email. Passwords and session tokens are absent from the configuration.")?;
            if prompts.supports_keychain() {
                prompts.line("Optionally save the password in this Mac's Keychain for future syncs. macOS may request access; a locked Keychain can require interaction.")?;
                while let Some(answer) =
                    cancelled(prompts.input("Save password in macOS Keychain? [y/N]: ")?)
                {
                    match answer.to_ascii_lowercase().as_str() {
                        "yes" | "y" => {
                            remember_login(cli, name, &mut profile, &secret, prompts)?;
                            break;
                        }
                        "" | "no" | "n" => break,
                        _ => prompts.line("Type yes or no. Please try again.")?,
                    }
                }
            }
            drop(secret);
            let config_flag = cli
                .config
                .as_ref()
                .map(|path| {
                    format!(
                        " --config '{}'",
                        path.to_string_lossy().replace('\'', "'\\''")
                    )
                })
                .unwrap_or_default();
            prompts.line(&format!(
                "Profile saved. Browse with: {}",
                safe_text(&format!("epost-cli{config_flag} --profile={name} browse"))
            ))?;
            prompts.line(&format!(
                "Download new documents with: {}",
                safe_text(&format!("epost-cli{config_flag} --profile={name} sync"))
            ))?;
            login_message(&profile, prompts)?;
            Ok(0)
        }
        Command::Login { remember } => {
            let name = cli
                .profile
                .as_deref()
                .ok_or(Error::Config("login requires --profile"))?;
            let previous = cli
                .selected_profile
                .as_ref()
                .ok_or(Error::Config("login requires a saved profile"))?;
            let mut profile = previous.clone();
            profile.login = Some(Login {
                username: username.clone(),
                method: LoginMethod::Prompt,
            });
            if *remember {
                prompts.line(
                    "Saving the password in this Mac's Keychain, as requested by --remember.",
                )?;
                profile.login.as_mut().unwrap().method = LoginMethod::MacosKeychain;
            }
            prompts.persist_profile(
                cli,
                name,
                &profile,
                Some(previous),
                if *remember { Some(&secret) } else { None },
            )?;
            drop(secret);
            login_message(&profile, prompts)?;
            Ok(0)
        }
        Command::Browse {
            output: explicit,
            page_size,
            location,
        } => {
            drop(secret);
            let Some(output) = destination(cli, explicit.as_deref(), prompts)? else {
                return Ok(0);
            };
            let mut exit = 0;
            if let Some(location) = location {
                let source = match location {
                    modern::Location::Inbox => Source::Inbox,
                    modern::Location::Archive => {
                        match archive_source(&mut client, prompts).await? {
                            Some(source) => source,
                            None => return Ok(0),
                        }
                    }
                    modern::Location::All => {
                        return Err(Error::Config(
                            "browse supports inbox or archive; use check all for combined retrieval",
                        ));
                    }
                };
                return Ok(browse_page(
                    cli,
                    &mut client,
                    &mailbox,
                    source,
                    &output,
                    *page_size,
                    prompts,
                )
                .await?
                .0);
            }
            loop {
                prompts.line("\n1 = inbox, 2 = archive, 3 = sync new documents, q = quit")?;
                let Some(answer) = cancelled(prompts.input("Action: ")?) else {
                    return Ok(exit);
                };
                match answer.to_ascii_lowercase().as_str() {
                    "1" | "inbox" => {
                        let (code, quit) = browse_page(
                            cli,
                            &mut client,
                            &mailbox,
                            Source::Inbox,
                            &output,
                            *page_size,
                            prompts,
                        )
                        .await?;
                        exit = exit.max(code);
                        if quit {
                            return Ok(exit);
                        }
                    }
                    "2" | "archive" => {
                        let Some(source) = archive_source(&mut client, prompts).await? else {
                            return Ok(exit);
                        };
                        let (code, quit) = browse_page(
                            cli,
                            &mut client,
                            &mailbox,
                            source,
                            &output,
                            *page_size,
                            prompts,
                        )
                        .await?;
                        exit = exit.max(code);
                        if quit {
                            return Ok(exit);
                        }
                    }
                    "3" | "sync" => {
                        let selected_scope = match &cli.selected_profile {
                            Some(profile) => profile.scope,
                            None => match scope(prompts)? {
                                Some(scope) => scope,
                                None => return Ok(exit),
                            },
                        };
                        let Some(answer) = cancelled(
                            prompts
                                .input("Scan this scope and download new documents? (yes/no): ")?,
                        ) else {
                            return Ok(exit);
                        };
                        if !matches!(answer.to_ascii_lowercase().as_str(), "yes" | "y") {
                            prompts.line("Sync cancelled. Choose sync again to retry.")?;
                            continue;
                        }
                        prompts.line("Scanning for new documents and verifying saved files…")?;
                        let (report, code) = sync(
                            cli,
                            &mut client,
                            &mailbox,
                            &ResolvedSyncArgs {
                                scope: selected_scope,
                                output: output.clone(),
                                page_size: 48,
                                max_pages: 1000,
                                max_documents: 100_000,
                            },
                        )
                        .await?;
                        exit = exit.max(code);
                        show_sync(&report, code, prompts)?;
                    }
                    _ => prompts.line("Choose 1, 2, 3 or q. Please try again.")?,
                }
            }
        }
        _ => Err(Error::Config("guided session requires setup or browse")),
    }
}

fn remember_login(
    cli: &Cli,
    name: &str,
    profile: &mut Profile,
    secret: &Secret,
    prompts: &mut impl Prompts,
) -> Result<()> {
    let previous = profile.clone();
    profile.login.as_mut().unwrap().method = LoginMethod::MacosKeychain;
    prompts.persist_profile(cli, name, profile, Some(&previous), Some(secret))
}

fn login_message(profile: &Profile, prompts: &mut impl Prompts) -> Result<()> {
    match profile.login.as_ref().map(|login| login.method) {
        Some(LoginMethod::MacosKeychain) => prompts.line("Account email remembered; future commands obtain the password from macOS Keychain. No authenticated session is saved."),
        _ => prompts.line("Account email remembered; future commands still need a password. Use epost login --remember for macOS Keychain, or your scheduler's secret mechanism."),
    }
}

async fn archive_source(
    client: &mut ApiClient,
    prompts: &mut impl Prompts,
) -> Result<Option<Source>> {
    let directories = client.directories().await?;
    prompts.line("Archive: 0 = root documents. Choose a folder number, or q to quit.")?;
    for (index, directory) in directories.iter().enumerate() {
        let unavailable = if directory
            .directory_id
            .as_deref()
            .is_none_or(|id| !usable_id(id))
        {
            " [unavailable through this API]"
        } else {
            ""
        };
        prompts.line(&format!(
            "  {}. {}{}",
            index + 1,
            safe_line(
                directory
                    .directory_name
                    .as_deref()
                    .unwrap_or("Unnamed folder")
            ),
            unavailable
        ))?;
    }
    loop {
        let Some(answer) = cancelled(prompts.input("Archive folder: ")?) else {
            return Ok(None);
        };
        if answer == "0" {
            return Ok(Some(Source::ArchiveRoot));
        }
        if let Some(number) = number(&answer, directories.len()) {
            if let Some(id) = directories[number - 1]
                .directory_id
                .as_ref()
                .filter(|id| usable_id(id))
            {
                return Ok(Some(Source::ArchiveDirectory(id.clone())));
            }
            prompts.line("That folder has no API selector. Choose another folder or root (0).")?;
        } else {
            prompts.line("Choose 0 or a listed folder number. Please try again.")?;
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn browse_page(
    cli: &Cli,
    client: &mut ApiClient,
    mailbox: &Mailbox,
    source: Source,
    output: &Path,
    page_size: u32,
    prompts: &mut impl Prompts,
) -> Result<(u8, bool)> {
    let mut offset = 0u32;
    let mut exit = 0;
    loop {
        let letters = pagination::page(client, &source, page_size, offset).await?;
        prompts.line(&format!(
            "\nShowing {} entries at offset {offset}. This page is not a full mailbox inventory.",
            letters.len()
        ))?;
        for (index, letter) in letters.iter().enumerate() {
            let date = letter
                .extra
                .get("receivedDateTime")
                .and_then(Value::as_str)
                .unwrap_or("No date");
            let title = letter
                .letter_title
                .as_deref()
                .or(letter.file_name.as_deref())
                .unwrap_or("Untitled document");
            let status = if registered_pending(letter) {
                " [requires action in ePost]"
            } else if letter.id.as_deref().is_none_or(|id| !usable_id(id)) {
                " [unavailable document ID]"
            } else {
                ""
            };
            prompts.line(&format!(
                "  {}. {} | {}{}",
                index + 1,
                safe_line(date),
                safe_line(title),
                status
            ))?;
        }
        loop {
            let Some(answer) = cancelled(
                prompts
                    .input("Document number to download, n = next page, b = back, q = quit: ")?,
            ) else {
                return Ok((exit, true));
            };
            match answer.to_ascii_lowercase().as_str() {
                "b" | "back" => return Ok((exit, false)),
                "n" | "next" => {
                    if letters.len() < page_size as usize {
                        prompts
                            .line("No next page was indicated. Choose a document, back or quit.")?;
                    } else {
                        offset = offset
                            .checked_add(page_size)
                            .filter(|n| *n <= i32::MAX as u32)
                            .ok_or(Error::Config("page offset safety limit reached"))?;
                        break;
                    }
                }
                _ => {
                    let Some(number) = number(&answer, letters.len()) else {
                        prompts.line(
                            "Choose a listed document number, n, b or q. Please try again.",
                        )?;
                        continue;
                    };
                    let Some(id) = letters[number - 1].id.as_deref().filter(|id| usable_id(id))
                    else {
                        prompts.line(
                            "This entry has no usable document ID and cannot be downloaded.",
                        )?;
                        continue;
                    };
                    let letter = client.letter(id).await?;
                    if letter.id.as_deref() != Some(id) {
                        return Err(Error::InvalidResponse(
                            "letter ID does not match selected document",
                        ));
                    }
                    let mut store = Store::open(output, &namespace(cli, mailbox)?)?;
                    let record = retrieve(client, &mut store, &letter, &source.label()).await?;
                    if !supported(&record) {
                        exit = exit.max(2);
                    }
                    show_document(&record, prompts)?;
                }
            }
        }
    }
}

fn show_document(record: &Retrieval, prompts: &mut impl Prompts) -> Result<()> {
    if record.status == "awaiting_manual_acceptance_or_status_verification" {
        return prompts.line("Requires action in the ePost app. Check its status and any required action there, then retry.");
    }
    if record.status == "failed" {
        prompts.line("Download unavailable; no content was saved.")?;
        for line in epost_cli::summary::document_attention(&value(record)?) {
            prompts.line(&line)?;
        }
        return Ok(());
    }
    if let Some(document) = &record.document {
        let label = if record.status == "already_saved" {
            "Already saved and verified"
        } else {
            "Saved"
        };
        prompts.line(&format!(
            "{label}: {}",
            safe_text(&document.path.to_string_lossy())
        ))?;
        if document.media_kind != "pdf" {
            prompts.line("Content was preserved, but is not a supported PDF. Check it manually (partial result).")?;
        }
    }
    Ok(())
}

fn usable_id(id: &str) -> bool {
    !id.trim().is_empty() && id != "." && id != ".." && !id.chars().any(char::is_control)
}

fn show_sync(report: &Value, code: u8, prompts: &mut impl Prompts) -> Result<()> {
    for line in epost_cli::summary::sync_summary(report, code) {
        prompts.line(&line)?;
    }
    let documents = report
        .get("documents")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    if code != 0 {
        let mut shown = 0;
        let mut additional = 0;
        for document in documents {
            let status = document.get("status").and_then(Value::as_str);
            let media = document
                .get("document")
                .and_then(|saved| saved.get("media_kind"))
                .and_then(Value::as_str);
            if !matches!(status, Some("saved_pdf" | "already_saved")) || media != Some("pdf") {
                if shown >= 20 {
                    additional += 1;
                    continue;
                }
                shown += 1;
                prompts.line(&format!(
                    "  Document {}: {} ({}).",
                    safe_line(
                        document
                            .get("letter_id")
                            .and_then(Value::as_str)
                            .unwrap_or("without ID")
                    ),
                    safe_line(status.unwrap_or("unknown result")),
                    safe_line(media.unwrap_or("no saved content"))
                ))?;
            }
        }
        if additional > 0 {
            prompts.line(&format!(
                "  {additional} more documents need attention. Use --json for the complete report."
            ))?;
        }
    }
    Ok(())
}
