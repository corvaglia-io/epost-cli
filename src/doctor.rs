//! Fixed, redacted diagnostics. Offline checks never load secrets or use DNS.

use super::*;
use epost_cli::destinations::{self, Destination, DestinationKind};
use epost_cli::{delivery, progress};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs,
    net::{IpAddr, ToSocketAddrs},
    path::Component,
    time::Duration,
};

#[derive(Args, Clone)]
pub struct DoctorArgs {
    /// Check only these named destinations; otherwise check every destination.
    #[arg(long = "destination")]
    pub destinations: Vec<String>,
    /// Check local configuration and files without network or credential reads.
    #[arg(long)]
    pub offline: bool,
}

struct Report {
    checks: Vec<Value>,
    offline: bool,
    network_attempted: bool,
}

impl Report {
    fn new(offline: bool) -> Self {
        Self {
            checks: Vec::new(),
            offline,
            network_attempted: false,
        }
    }

    fn add(
        &mut self,
        component: &'static str,
        status: &'static str,
        message: &'static str,
        destination: Option<&str>,
        details: Option<Value>,
    ) {
        progress::publish_update(
            progress::Telemetry::Diagnostic {
                component: destination
                    .map(|name| format!("{name}/{component}"))
                    .unwrap_or_else(|| component.into()),
                status: match status {
                    "ok" => progress::DiagnosticStatus::Ok,
                    "warning" => progress::DiagnosticStatus::Warning,
                    "error" => progress::DiagnosticStatus::Error,
                    _ => progress::DiagnosticStatus::NotChecked,
                },
            },
            &format!("Doctor: {component} — {message}"),
        );
        let mut row = json!({"component": component, "status": status, "message": message});
        if let Some(name) = destination {
            row["destination"] = json!(name);
        }
        if let Some(details) = details {
            row["details"] = details;
        }
        self.checks.push(row);
    }

    fn finish(self) -> (Value, u8) {
        let healthy = !self
            .checks
            .iter()
            .any(|row| matches!(row["status"].as_str(), Some("warning" | "error")));
        (
            json!({"healthy": healthy, "offline": self.offline, "network_checks_attempted": self.network_attempted, "mailbox_coverage_verified": false, "checks": self.checks}),
            if healthy { 0 } else { 2 },
        )
    }
}

pub async fn run(cli: &Cli, args: &DoctorArgs) -> Result<(Value, u8)> {
    let mut report = Report::new(args.offline);
    let mut diagnostic = diagnostic_cli(cli);
    progress::publish_line("Doctor: checking profile configuration");
    let path = match config::path(cli.config.as_deref()) {
        Ok(path) => path,
        Err(_) => {
            report.add("configuration", "error", "Configuration path is unavailable or unsafe; supply a valid absolute --config path.", None, None);
            return Ok(report.finish());
        }
    };
    let default = match (config::list_profiles(&path), config::default_profile(&path)) {
        (Ok(profiles), Ok(default)) => {
            if profiles.is_empty() {
                report.add(
                    "configuration",
                    "error",
                    "No saved profiles are available; run setup before checking this account.",
                    None,
                    None,
                );
            } else {
                report.add("configuration", "ok", "Saved profile configuration and default references are valid.", None, Some(json!({"profiles": profiles.len(), "saved_or_sole_default": default.is_some()})));
            }
            default
        }
        _ => {
            report.add("configuration", "error", "Profile configuration is corrupt, unsupported or unsafe; repair it before retrying.", None, None);
            return Ok(report.finish());
        }
    };
    diagnostic.profile = diagnostic.profile.or(default);
    diagnostic.selected_profile = None;
    if diagnostic.profile.is_none() {
        report.add("source_profile", "error", "No source profile is selected; run setup or choose a profile with --profile or profile use.", None, None);
        return Ok(report.finish());
    }
    if load_profile(&mut diagnostic).is_err() {
        report.add("source_profile", "error", "Selected profile is unavailable or conflicts with explicit mailbox flags; choose a saved profile without selection overrides.", None, None);
        return Ok(report.finish());
    }
    report.add(
        "source_profile",
        "ok",
        "An explicit saved mailbox profile is selected.",
        None,
        None,
    );

    progress::publish_line("Doctor: checking local source store");
    let store = local_store(&diagnostic, &mut report);
    let selected = store.as_ref().and_then(|(_, store)| match destinations::select(store, &args.destinations) {
        Ok(selected) => {
            report.add("destinations", "ok", "Destination registry and requested selections are valid.", None, Some(json!({"configured_selection": selected.len()})));
            Some(selected)
        }
        Err(_) => {
            report.add("destinations", "error", "Destination configuration or selection is invalid; inspect destination list and repair the selected names or configuration.", None, None);
            None
        }
    });
    if let Some((_, store)) = &store
        && let Some(selected) = &selected
    {
        for destination in selected {
            local_destination(store, destination, &mut report);
        }
    }
    if args.offline {
        report.add("source_credentials", "not_checked", "Source sign-in was not checked in offline mode; local checks do not establish live access.", None, None);
        report.add("network", "not_checked", "DNS, TLS, destination credentials and remote catalog bindings were not checked in offline mode.", None, None);
        return Ok(report.finish());
    }

    progress::publish_line("Doctor: checking source access without prompts");
    source_access(&diagnostic, &mut report).await;
    if let Some((root, store)) = &store
        && let Some(selected) = &selected
    {
        for destination in selected {
            if matches!(destination.kind, DestinationKind::Paperless(_)) {
                progress::publish_line("Doctor: checking destination network and access");
                remote_destination(&diagnostic, root, store, destination, &mut report).await;
            }
        }
    }
    Ok(report.finish())
}

fn diagnostic_cli(cli: &Cli) -> Cli {
    let mut diagnostic = cli.clone();
    diagnostic.non_interactive = true;
    // Diagnose an unattended run, without reusing an interactive session's
    // secrets or masking changed profile/destination credential bindings.
    diagnostic.prepared_credentials = None;
    diagnostic.prepared_paperless_token = None;
    diagnostic.prepared_destination_tokens.clear();
    diagnostic.destinations_prepared = false;
    diagnostic.prepared_destination_failures.clear();
    diagnostic
}

fn local_store(cli: &Cli, report: &mut Report) -> Option<(PathBuf, Store)> {
    let root = match output(cli, None) {
        Ok(root) => root,
        Err(_) => {
            report.add("source_store", "error", "The selected profile has no valid document store; run setup with an output directory.", None, None);
            return None;
        }
    };
    let initialized = (|| {
        private_directory(&root)?;
        let selected = mailbox(cli)?;
        let namespace = namespace(cli, &selected)?;
        let namespace_root = root.join(hex::encode(Sha256::digest(namespace.as_bytes())));
        private_directory(&namespace_root)?;
        private_directory(&namespace_root.join("data"))?;
        private_lock(&namespace_root.join(".lock"))?;
        Ok::<(), Error>(())
    })();
    if initialized.is_err() {
        report.add("source_store", "error", "Source store is missing, not initialized or not private; use a private directory, then run check to initialize it.", None, None);
        return None;
    }
    let (root, store) = match destination_cli::open_store(cli, None) {
        Ok(store) => store,
        Err(_) => {
            report.add("source_store", "error", "Source store is locked, corrupt or unsafe; finish other runs and repair local storage before retrying.", None, None);
            return None;
        }
    };
    let verified = (|| {
        let documents = store.saved_documents()?;
        // This fixed lookup validates the complete optional metadata registry.
        // Neither its result nor any account/document identifier is printed.
        let _ = store.source_metadata("doctor-local-integrity-check")?;
        for document in &documents {
            let _ = store.read_saved(&document.letter_id)?;
        }
        Ok::<usize, Error>(documents.len())
    })();
    match verified {
        Ok(count) => report.add("source_store", "ok", "Private local storage, manifest, metadata and saved document bytes passed integrity checks.", None, Some(json!({"saved_documents": count}))),
        Err(_) => report.add("source_store", "error", "Local saved documents or metadata are missing, corrupt or unsafe; run verify and repair the affected local store.", None, None),
    }
    Some((root, store))
}

fn local_destination(store: &Store, destination: &Destination, report: &mut Report) {
    let name = Some(destination.name.as_str());
    match delivery::destination_status(store, destination) {
        Ok(status) => {
            let attention = status.pending + status.uncertain + status.task_failed;
            report.add("delivery_state", if attention > 0 {"warning"} else {"ok"}, if attention > 0 {"Delivery has pending, uncertain or failed work; run deliver or verify to reconcile it before retrying uncertain uploads."} else {"Delivery history is valid and has no pending or uncertain uploads."}, name, Some(json!({"pending": status.pending, "uncertain": status.uncertain, "task_failed": status.task_failed, "not_submitted": status.not_submitted, "verified": status.verified})));
        }
        Err(_) => report.add("delivery_state", "error", "Delivery history is corrupt or conflicts with this destination; repair it before attempting delivery.", name, None),
    }
    if let DestinationKind::Folder { path } = &destination.kind {
        if path.starts_with(store.root_path()) {
            report.add("folder_storage", "error", "Folder destination overlaps the source store; choose a separate private destination directory.", name, None);
        } else if private_directory(path).is_err() {
            report.add("folder_storage", "error", "Folder destination is missing, unsafe or has broad permissions; create a separate private directory and retry.", name, None);
        } else {
            report.add("folder_storage", "ok", "Folder destination is a separate existing private directory; no files were copied or written.", name, None);
        }
    }
}

fn private_directory(path: &Path) -> Result<()> {
    if !path.is_absolute()
        || path.parent().is_none()
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
    {
        return Err(Error::Storage("diagnostic directory path is unsafe"));
    }
    let mut current = PathBuf::new();
    for part in path.components() {
        current.push(part.as_os_str());
        let metadata = fs::symlink_metadata(&current)
            .map_err(|_| Error::Storage("diagnostic directory is unavailable"))?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(Error::Storage("diagnostic directory is unsafe"));
        }
    }
    let metadata = fs::symlink_metadata(path)
        .map_err(|_| Error::Storage("diagnostic directory is unavailable"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.mode() & 0o077 != 0 || metadata.mode() & 0o700 != 0o700 {
            return Err(Error::Storage(
                "diagnostic directory is not private and accessible",
            ));
        }
    }
    #[cfg(not(unix))]
    if metadata.permissions().readonly() {
        return Err(Error::Storage("diagnostic directory is not writable"));
    }
    Ok(())
}

fn private_lock(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|_| Error::Storage("diagnostic store lock is unavailable"))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(Error::Storage("diagnostic store lock is unsafe"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 || metadata.mode() & 0o077 != 0 || metadata.mode() & 0o600 != 0o600
        {
            return Err(Error::Storage("diagnostic store lock is unsafe"));
        }
    }
    Ok(())
}

async fn source_access(cli: &Cli, report: &mut Report) {
    if progress::stop_requested() {
        report.add(
            "source_access",
            "warning",
            "Doctor stopped before the next network check; rerun to complete diagnostics.",
            None,
            None,
        );
        return;
    }
    let selected = match mailbox(cli) {
        Ok(selected) => selected,
        Err(_) => return,
    };
    let mut source = match client(cli, Some(selected.clone())) {
        Ok(source) => source,
        Err(_) => {
            report.add("source_access", "error", "Source transport configuration is invalid; repair timeout bounds or the selected environment.", None, None);
            return;
        }
    };
    let origin = cli
        .mock_url
        .as_deref()
        .unwrap_or(environment(cli).base_url());
    report.network_attempted = true;
    match address_categories(origin, cli).await {
        Ok(categories) => report.add(
            "source_dns",
            "ok",
            "Source hostname resolved; only coarse address categories are reported.",
            None,
            Some(json!({"address_categories": categories})),
        ),
        Err(_) => {
            report.add("source_dns", "error", "Source DNS could not be resolved within the deadline; check DNS or network settings and retry.", None, None);
            return;
        }
    }
    if authenticate(cli, &mut source, &selected).await.is_err() {
        report.add("source_credentials", "error", "Source sign-in failed without prompting; supply runtime credentials or select this profile and run login to repair its saved sign-in.", None, None);
        return;
    }
    report.add(
        "source_credentials",
        "ok",
        "Source credentials were available without prompts or interactive Keychain approval.",
        None,
        None,
    );
    match source.inbox_unread_count().await {
        Ok(_) => report.add("source_access", "ok", "A read-only inbox count request succeeded; this does not establish complete mailbox coverage.", None, None),
        Err(_) => report.add("source_access", "error", "Source inbox access failed; check credentials, tenant access, TLS trust and connectivity before retrying.", None, None),
    }
}

async fn remote_destination(
    cli: &Cli,
    root: &Path,
    store: &Store,
    destination: &Destination,
    report: &mut Report,
) {
    let DestinationKind::Paperless(config) = &destination.kind else {
        return;
    };
    let name = Some(destination.name.as_str());
    if progress::stop_requested() {
        report.add(
            "destination_access",
            "warning",
            "Doctor stopped before the next destination check; rerun to complete diagnostics.",
            name,
            None,
        );
        return;
    }
    report.network_attempted = true;
    match address_categories(&config.target.url, cli).await {
        Ok(categories) => report.add(
            "destination_dns",
            "ok",
            "Destination hostname resolved; only coarse address categories are reported.",
            name,
            Some(json!({"address_categories": categories})),
        ),
        Err(_) => {
            report.add("destination_dns", "error", "Destination DNS or URL validation failed; check the configured installation and DNS settings.", name, None);
            return;
        }
    }
    match unauthenticated_probe(cli, &config.target.url).await {
        Probe::Reachable => report.add("destination_transport", "ok", "Configured API endpoint is reachable without following redirects; no token was sent during the probe.", name, None),
        Probe::AccessGateway => {
            report.add("destination_transport", "warning", "An access gateway or web sign-in page blocks the API; configure API service access and retry. No destination token was loaded or sent.", name, None);
            return;
        }
        Probe::Failed => {
            report.add("destination_transport", "error", "Configured API endpoint failed its bounded probe; check the installation path, TLS certificate trust, firewall and connectivity. No destination token was loaded or sent.", name, None);
            return;
        }
    }
    let remote = match destination_cli::client_for(cli, root, store, destination) {
        Ok(remote) => remote,
        Err(_) => {
            report.add("destination_credentials", "error", "Destination credentials are unavailable without interaction; supply the configured runtime token or authorize its saved Keychain entry in an interactive session.", name, None);
            return;
        }
    };
    match destination_cli::validate_paperless_catalogs(&remote, config).await {
        Ok(()) => report.add("destination_catalogs", "ok", "Authenticated destination access and configured tag, custom field and document type bindings are valid.", name, None),
        Err(_) => report.add("destination_catalogs", "error", "Destination authentication or catalog bindings failed; check the token permissions and configured tag, custom field and document type IDs.", name, None),
    }
}

async fn address_categories(origin: &str, cli: &Cli) -> Result<Vec<&'static str>> {
    let url = reqwest::Url::parse(origin).map_err(|_| Error::Network)?;
    let host = url
        .host_str()
        .ok_or(Error::Network)?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_owned();
    if let Ok(address) = host.parse::<IpAddr>() {
        if cli.mock_url.is_some() && !address.is_loopback() {
            return Err(Error::Network);
        }
        return Ok(vec![address_category(address)]);
    }
    if cli.mock_url.is_some() {
        return Err(Error::Network);
    }
    let port = url.port_or_known_default().ok_or(Error::Network)?;
    let resolver = tokio::task::spawn_blocking(move || {
        (host.as_str(), port).to_socket_addrs().map(|addresses| {
            addresses
                .take(32)
                .map(|address| address_category(address.ip()))
                .collect::<BTreeSet<_>>()
        })
    });
    // A timed-out OS resolver may finish later; it receives only the configured
    // hostname and port, never credentials or response data.
    let categories =
        tokio::time::timeout(Duration::from_secs(cli.timeout_seconds.min(10)), resolver)
            .await
            .map_err(|_| Error::Network)?
            .map_err(|_| Error::Network)?
            .map_err(|_| Error::Network)?;
    if categories.is_empty() {
        return Err(Error::Network);
    }
    Ok(categories.into_iter().collect())
}

fn address_category(address: IpAddr) -> &'static str {
    match address {
        IpAddr::V4(address)
            if address.is_private()
                || address.is_loopback()
                || address.is_link_local()
                || address.is_unspecified()
                || address.is_multicast()
                || address.is_broadcast()
                || address.is_documentation()
                || address.octets()[0] == 0
                || address.octets()[0] >= 240
                || (address.octets()[0] == 100 && (64..=127).contains(&address.octets()[1]))
                || (address.octets()[0] == 198 && matches!(address.octets()[1], 18 | 19)) =>
        {
            "ipv4_private_or_special"
        }
        IpAddr::V4(_) => "ipv4_public",
        IpAddr::V6(address)
            if address.is_loopback()
                || address.is_unspecified()
                || address.is_multicast()
                || address.is_unique_local()
                || address.is_unicast_link_local()
                || (address.segments()[0] == 0x2001 && address.segments()[1] == 0x0db8)
                || address.to_ipv4_mapped().is_some_and(|address| {
                    address_category(IpAddr::V4(address)) == "ipv4_private_or_special"
                }) =>
        {
            "ipv6_private_or_special"
        }
        IpAddr::V6(_) => "ipv6_public",
    }
}

enum Probe {
    Reachable,
    AccessGateway,
    Failed,
}

async fn unauthenticated_probe(cli: &Cli, origin: &str) -> Probe {
    let normalized = match epost_cli::paperless::normalize_url(origin, cli.mock_url.is_some()) {
        Ok(url) => url,
        Err(_) => return Probe::Failed,
    };
    let url = match reqwest::Url::parse(&normalized).and_then(|url| url.join("api/tags/")) {
        Ok(url) => url,
        Err(_) => return Probe::Failed,
    };
    if cli.mock_url.is_some()
        && url
            .host_str()
            .and_then(|host| {
                host.trim_start_matches('[')
                    .trim_end_matches(']')
                    .parse::<IpAddr>()
                    .ok()
            })
            .is_none_or(|address| !address.is_loopback())
    {
        return Probe::Failed;
    }
    let http = match reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(cli.timeout_seconds.min(10)))
        .connect_timeout(Duration::from_secs(cli.timeout_seconds.min(10)))
        .build()
    {
        Ok(http) => http,
        Err(_) => return Probe::Failed,
    };
    let response = match http.get(url).query(&[("page_size", "1")]).send().await {
        Ok(response) => response,
        Err(_) => return Probe::Failed,
    };
    // Inspect headers/status only; never consume or display a response body.
    let status = response.status();
    let cloudflare = response.headers().contains_key("cf-ray")
        || response
            .headers()
            .get(reqwest::header::SERVER)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|server| server.to_ascii_lowercase().contains("cloudflare"));
    let html = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|kind| kind.to_ascii_lowercase().starts_with("text/html"));
    if status.is_redirection()
        || (cloudflare && matches!(status.as_u16(), 401 | 403))
        || (status.is_success() && html)
    {
        return Probe::AccessGateway;
    }
    if response
        .content_length()
        .is_some_and(|bytes| bytes > 64 * 1024)
    {
        return Probe::Failed;
    }
    if status.is_success() || matches!(status.as_u16(), 401 | 403) {
        Probe::Reachable
    } else {
        Probe::Failed
    }
}

pub fn summary(report: &Value) -> Vec<String> {
    let mut lines = vec![if report["offline"].as_bool() == Some(true) {
        "Doctor: local checks only; live access was not checked.".to_owned()
    } else {
        "Doctor: bounded checks completed without interactive prompts.".to_owned()
    }];
    if let Some(checks) = report["checks"].as_array() {
        for check in checks {
            let component = check["component"].as_str().unwrap_or("diagnostic");
            let status = check["status"].as_str().unwrap_or("error");
            let message = check["message"]
                .as_str()
                .unwrap_or("Diagnostic unavailable.");
            let destination = check["destination"]
                .as_str()
                .map(|name| format!(" ({name})"))
                .unwrap_or_default();
            lines.push(format!("{component}{destination}: {status} — {message}"));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unattended_diagnostics_clear_all_interactive_caches_on_the_clone_only() {
        let mut cli = Cli::try_parse_from(["epost", "doctor", "--offline"]).unwrap();
        cli.prepared_credentials = Some((
            "synthetic-doctor@example.invalid".into(),
            Secret::new("synthetic-source-password".into()),
        ));
        cli.prepared_paperless_token = Some(paperless_cli::PreparedToken::new(
            "synthetic-legacy-identity".into(),
            Secret::new("synthetic-paperless-token".into()),
        ));
        cli.prepared_destination_tokens.insert(
            "synthetic-destination".into(),
            paperless_cli::PreparedToken::new(
                "synthetic-destination-identity".into(),
                Secret::new("synthetic-destination-token".into()),
            ),
        );
        cli.destinations_prepared = true;
        cli.prepared_destination_failures
            .insert("synthetic-destination".into());
        let diagnostic = diagnostic_cli(&cli);
        assert!(diagnostic.non_interactive);
        assert!(diagnostic.prepared_credentials.is_none());
        assert!(diagnostic.prepared_paperless_token.is_none());
        assert!(diagnostic.prepared_destination_tokens.is_empty());
        assert!(!diagnostic.destinations_prepared);
        assert!(diagnostic.prepared_destination_failures.is_empty());
        assert!(cli.prepared_credentials.is_some());
        assert!(cli.prepared_paperless_token.is_some());
        assert_eq!(cli.prepared_destination_tokens.len(), 1);
        assert!(cli.destinations_prepared);
        assert_eq!(cli.prepared_destination_failures.len(), 1);
    }

    #[test]
    fn skipped_online_checks_do_not_make_valid_offline_inventory_unhealthy() {
        let mut report = Report::new(true);
        report.add("source_store", "ok", "Local integrity checked.", None, None);
        report.add("source_credentials", "not_checked", "Offline.", None, None);
        let (value, code) = report.finish();
        assert_eq!(code, 0);
        assert_eq!(value["healthy"], true);
        assert_eq!(value["network_checks_attempted"], false);
        for status in ["warning", "error"] {
            let mut report = Report::new(true);
            report.add("delivery_state", status, "Needs attention.", None, None);
            let (value, code) = report.finish();
            assert_eq!(code, 2);
            assert_eq!(value["healthy"], false);
        }
    }

    #[test]
    fn address_diagnostics_emit_categories_and_never_literal_addresses() {
        for (address, category) in [
            ("127.0.0.1", "ipv4_private_or_special"),
            ("10.1.2.3", "ipv4_private_or_special"),
            ("100.64.0.1", "ipv4_private_or_special"),
            ("192.0.2.1", "ipv4_private_or_special"),
            ("8.8.8.8", "ipv4_public"),
            ("::1", "ipv6_private_or_special"),
            ("fd00::1", "ipv6_private_or_special"),
            ("2001:db8::1", "ipv6_private_or_special"),
            ("::ffff:127.0.0.1", "ipv6_private_or_special"),
            ("2606:4700:4700::1111", "ipv6_public"),
        ] {
            let actual = address_category(address.parse().unwrap());
            assert_eq!(actual, category);
            assert!(!actual.contains(address));
        }
    }

    #[cfg(unix)]
    #[test]
    fn directory_checks_reject_public_permissions_and_symlinks_without_chmod() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let folder = base.join("synthetic-folder");
        fs::create_dir(&folder).unwrap();
        fs::set_permissions(&folder, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(private_directory(&folder).is_err());
        assert_eq!(
            fs::metadata(&folder).unwrap().permissions().mode() & 0o777,
            0o755
        );
        fs::set_permissions(&folder, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(private_directory(&folder).is_ok());
        let alias = base.join("synthetic-alias");
        symlink(&folder, &alias).unwrap();
        assert!(private_directory(&alias).is_err());
    }
}
