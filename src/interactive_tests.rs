use super::*;
use crate::interactive::{Prompts, safe_line};
use std::{collections::VecDeque, fs};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const EMAIL: &str = "invented-guided-user@example.invalid";
const PASSWORD: &str = "synthetic-guided-password";
const TOKEN: &str = "synthetic-guided-token";
const PDF: &[u8] = b"%PDF-1.4\n1 0 obj\n<<>>\nendobj\n%%EOF\n";

struct Answers {
    inputs: VecDeque<String>,
    credential_calls: usize,
    displayed: Vec<String>,
}

impl Answers {
    fn new(inputs: &[&str]) -> Self {
        Self {
            inputs: inputs.iter().map(|answer| (*answer).to_owned()).collect(),
            credential_calls: 0,
            displayed: Vec::new(),
        }
    }

    fn text(&self) -> String {
        self.displayed.join("\n")
    }

    fn assert_safe(&self) {
        assert!(!self.text().contains(PASSWORD));
        assert!(!self.text().contains(TOKEN));
    }
}

impl Prompts for Answers {
    fn input(&mut self, prompt: &str) -> Result<Option<String>> {
        self.displayed.push(prompt.to_owned());
        Ok(self.inputs.pop_front())
    }

    fn credentials(&mut self, _: &Cli) -> Result<(String, Secret)> {
        self.credential_calls += 1;
        Ok((EMAIL.into(), Secret::new(PASSWORD.into())))
    }

    fn line(&mut self, text: &str) -> Result<()> {
        self.displayed.push(text.to_owned());
        Ok(())
    }
}

fn scratch() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("epost-cli-guided-synthetic-")
        .tempdir_in(std::env::temp_dir().canonicalize().unwrap())
        .unwrap()
}

fn cli(server: &MockServer, directory: &Path, arguments: &[&str]) -> Cli {
    let mut arguments_all = vec![
        "epost-cli".to_owned(),
        "--mock-url".into(),
        server.uri(),
        "--timeout-seconds".into(),
        "3".into(),
        "--retries".into(),
        "0".into(),
        "--config".into(),
        directory.join("config.json").to_string_lossy().into_owned(),
    ];
    arguments_all.extend(arguments.iter().map(|argument| (*argument).to_owned()));
    Cli::try_parse_from(arguments_all).unwrap()
}

fn saved_profile(directory: &Path) {
    config::save_profile(
        &directory.join("config.json"),
        "personal",
        &Profile {
            mailbox: Mailbox {
                tenant_id: "synthetic-one".into(),
                company_id: 17,
                kind: MailboxKind::Private,
            },
            environment: Environment::Production,
            output: directory.join("documents"),
            scope: Scope::Inbox,
        },
    )
    .unwrap();
}

fn profile_cli(server: &MockServer, directory: &Path, extra: &[&str]) -> Cli {
    saved_profile(directory);
    let mut arguments = vec!["--profile", "personal", "browse"];
    arguments.extend(extra);
    let mut selected = cli(server, directory, &arguments);
    load_profile(&mut selected).unwrap();
    selected
}

async fn auth(server: &MockServer, tenants: Value, login_count: u64) {
    Mock::given(method("POST"))
        .and(path("/core/latest/tenants"))
        .respond_with(ResponseTemplate::new(200).set_body_json(tenants))
        .expect(1)
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/core/latest/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": TOKEN, "refreshToken": "synthetic-guided-refresh",
            "expires_in": 3600, "refresh_expires_in": 7200, "token_type": "Bearer"
        })))
        .expect(login_count)
        .mount(server)
        .await;
}

async fn standard_auth(server: &MockServer) {
    auth(server, json!([{ "tenant_id": "synthetic-one", "company_id": 17, "company_name": "Invented mailbox" }]), 1).await;
}

async fn inbox(server: &MockServer, letters: Value, count: u64) {
    Mock::given(method("GET"))
        .and(path("/epost/v2/letters"))
        .respond_with(ResponseTemplate::new(200).set_body_json(letters))
        .expect(count)
        .mount(server)
        .await;
}

async fn letter(server: &MockServer, id: &str, metadata: Value, count: u64) {
    Mock::given(method("GET"))
        .and(path(format!("/epost/v2/letters/{id}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(metadata))
        .expect(count)
        .mount(server)
        .await;
}

fn assert_read_only(requests: &[Request]) {
    for request in requests {
        let endpoint = request.url.path();
        assert!(
            request.method.as_str() == "POST"
                && matches!(endpoint, "/core/latest/tenants" | "/core/latest/token")
                || request.method.as_str() == "GET"
                    && (matches!(
                        endpoint,
                        "/epost/v2/letters"
                            | "/epost/v2/archives/directories"
                            | "/epost/v2/archives/letters"
                    ) || endpoint.starts_with("/epost/v2/letters/")),
            "unexpected method/path {} {}",
            request.method,
            endpoint
        );
    }
}

#[tokio::test]
async fn setup_retries_kind_and_tenant_choices_then_saves_only_selected_settings() {
    let server = MockServer::start().await;
    let temp = scratch();
    auth(
        &server,
        json!([
            { "tenant_id": "synthetic-one", "company_id": 17, "company_name": "Invented one" },
            { "tenant_id": "synthetic-two", "company_id": 23, "company_name": "Invented two" }
        ]),
        1,
    )
    .await;
    let documents = temp.path().join("documents");
    let cli = cli(
        &server,
        temp.path(),
        &[
            "setup",
            "--output",
            documents.to_str().unwrap(),
            "--scope",
            "both",
        ],
    );
    let mut answers = Answers::new(&["privte", "Business", "0", "99", "two", "2"]);
    assert_eq!(interactive::run(&cli, &mut answers).await.unwrap(), 0);
    assert_eq!(answers.credential_calls, 1);
    assert!(answers.inputs.is_empty());
    assert!(answers.text().matches("Please try again").count() >= 4);
    let saved = config::load_profile(&temp.path().join("config.json"), "personal").unwrap();
    assert_eq!(saved.mailbox.tenant_id, "synthetic-two");
    assert_eq!(saved.mailbox.company_id, 23);
    assert_eq!(saved.mailbox.kind, MailboxKind::Business);
    assert_eq!(saved.output, documents);
    assert_eq!(saved.scope, Scope::Both);
    let raw = fs::read_to_string(temp.path().join("config.json")).unwrap();
    for secret in [EMAIL, PASSWORD, TOKEN, "synthetic-guided-refresh"] {
        assert!(!raw.contains(secret));
    }
    let requests = server.received_requests().await.unwrap();
    let login = requests
        .iter()
        .find(|request| request.url.path() == "/core/latest/token")
        .unwrap();
    let form: Vec<_> = reqwest::Url::parse(&format!(
        "http://127.0.0.1/?{}",
        String::from_utf8_lossy(&login.body)
    ))
    .unwrap()
    .query_pairs()
    .into_owned()
    .collect();
    assert!(form.contains(&("tenant_id".into(), "synthetic-two".into())));
    assert!(form.contains(&("company_id".into(), "23".into())));
    assert_eq!(requests.len(), 2);
    assert_read_only(&requests);
    answers.assert_safe();
}

#[tokio::test]
async fn setup_retries_relative_destination_and_scope_answers() {
    let server = MockServer::start().await;
    let temp = scratch();
    standard_auth(&server).await;
    let documents = temp.path().join("documents");
    let cli = cli(&server, temp.path(), &["setup"]);
    let mut answers = Answers::new(&[
        "private",
        "1",
        "relative/folder",
        "/synthetic/../bad",
        documents.to_str().unwrap(),
        "everything",
        "3",
    ]);
    assert_eq!(interactive::run(&cli, &mut answers).await.unwrap(), 0);
    assert!(answers.inputs.is_empty());
    let saved = config::load_profile(&temp.path().join("config.json"), "personal").unwrap();
    assert_eq!(saved.output, documents);
    assert_eq!(saved.scope, Scope::Both);
    assert!(answers.text().matches("Please try again").count() >= 3);
}

#[tokio::test]
async fn one_discovered_tenant_still_requires_selection_and_eof_cancels_cleanly() {
    let server = MockServer::start().await;
    let temp = scratch();
    auth(
        &server,
        json!([{ "tenant_id": "synthetic-one", "company_id": 17 }]),
        0,
    )
    .await;
    let cli = cli(&server, temp.path(), &["setup"]);
    let mut answers = Answers::new(&["private"]);
    assert_eq!(interactive::run(&cli, &mut answers).await.unwrap(), 0);
    assert!(answers.text().contains("Mailbox number:"));
    assert!(!temp.path().join("config.json").exists());
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    assert_eq!(answers.credential_calls, 1);
}

#[tokio::test]
async fn existing_profile_is_kept_after_retrying_the_replace_question() {
    let server = MockServer::start().await;
    let temp = scratch();
    saved_profile(temp.path());
    standard_auth(&server).await;
    let previous = fs::read(temp.path().join("config.json")).unwrap();
    let documents = temp.path().join("different-output");
    let cli = cli(
        &server,
        temp.path(),
        &[
            "setup",
            "--output",
            documents.to_str().unwrap(),
            "--scope",
            "both",
        ],
    );
    let mut answers = Answers::new(&["2", "1", "perhaps", "no"]);
    assert_eq!(interactive::run(&cli, &mut answers).await.unwrap(), 0);
    assert_eq!(fs::read(temp.path().join("config.json")).unwrap(), previous);
    assert!(answers.text().contains("Profile kept"));
    assert!(!documents.exists());
}

#[tokio::test]
async fn profile_browsing_downloads_by_number_and_reuses_one_login_for_sync() {
    let server = MockServer::start().await;
    let temp = scratch();
    standard_auth(&server).await;
    inbox(&server, json!([{ "id": "synthetic-document", "letterTitle": "Invented title", "receivedDateTime": "2026-01-02T03:04:05Z" }]), 2).await;
    letter(
        &server,
        "synthetic-document",
        json!({ "id": "synthetic-document" }),
        2,
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/epost/v2/letters/synthetic-document/content"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(PDF)
                .insert_header("content-type", "application/pdf"),
        )
        .expect(1)
        .mount(&server)
        .await;
    let cli = profile_cli(&server, temp.path(), &[]);
    let mut answers = Answers::new(&[
        "bad-action",
        "1",
        "bad-document",
        "1",
        "1",
        "b",
        "3",
        "yes",
        "q",
    ]);
    assert_eq!(interactive::run(&cli, &mut answers).await.unwrap(), 0);
    assert_eq!(answers.credential_calls, 1);
    assert!(answers.inputs.is_empty());
    assert!(answers.text().contains("Invented title"));
    assert!(answers.text().contains("2026-01-02T03:04:05Z"));
    assert!(answers.text().contains("Already saved and verified"));
    let selected_mailbox = mailbox(&cli).unwrap();
    let mut saved_store = Store::open(
        &temp.path().join("documents"),
        &namespace(&cli, &selected_mailbox).unwrap(),
    )
    .unwrap();
    let saved = saved_store
        .verify_existing("synthetic-document", "inbox")
        .unwrap()
        .unwrap();
    assert!(answers.text().contains(saved.path.to_str().unwrap()));
    assert!(answers.text().contains("0 new PDFs, 1 already saved"));
    assert!(!answers.text().contains("Mailbox number:"));
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 7);
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.url.path() == "/core/latest/token")
            .count(),
        1
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.url.path() == "/core/latest/tenants")
            .count(),
        1
    );
    assert_read_only(&requests);
    answers.assert_safe();
}

#[tokio::test]
async fn numbered_pending_registered_document_reports_partial_without_content_or_acceptance() {
    let server = MockServer::start().await;
    let temp = scratch();
    standard_auth(&server).await;
    let pending = json!({ "id": "synthetic-pending", "letterTitle": "Invented registered item", "eLetterRegistered": true, "registeredStatus": "PENDING" });
    inbox(&server, json!([pending]), 1).await;
    letter(&server, "synthetic-pending", pending, 1).await;
    let cli = profile_cli(&server, temp.path(), &[]);
    let mut answers = Answers::new(&["1", "1", "q"]);
    assert_eq!(interactive::run(&cli, &mut answers).await.unwrap(), 2);
    assert!(answers.text().contains("Requires action in the ePost app"));
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 4);
    assert!(
        !requests
            .iter()
            .any(|request| request.url.path().ends_with("/content"))
    );
    assert_read_only(&requests);
    answers.assert_safe();
}

#[tokio::test]
async fn numbered_download_rejects_metadata_for_a_different_document() {
    let server = MockServer::start().await;
    let temp = scratch();
    standard_auth(&server).await;
    inbox(&server, json!([{ "id": "synthetic-selected" }]), 1).await;
    letter(
        &server,
        "synthetic-selected",
        json!({ "id": "synthetic-unexpected" }),
        1,
    )
    .await;
    let cli = profile_cli(&server, temp.path(), &[]);
    let mut answers = Answers::new(&["1", "1"]);
    assert!(matches!(
        interactive::run(&cli, &mut answers).await,
        Err(Error::InvalidResponse(_))
    ));
    assert!(!temp.path().join("documents").exists());
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 4);
    assert!(
        !requests
            .iter()
            .any(|request| request.url.path().ends_with("/content"))
    );
    assert_read_only(&requests);
}

#[tokio::test]
async fn branded_archive_folder_retries_without_silently_selecting_root() {
    let server = MockServer::start().await;
    let temp = scratch();
    standard_auth(&server).await;
    Mock::given(method("GET"))
        .and(path("/epost/v2/archives/directories"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!([{ "directoryId": "", "directoryName": "Invented branded folder" }]),
        ))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/epost/v2/archives/letters"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(1)
        .mount(&server)
        .await;
    let cli = profile_cli(&server, temp.path(), &[]);
    let mut answers = Answers::new(&["2", "1", "unknown", "0", "q"]);
    assert_eq!(interactive::run(&cli, &mut answers).await.unwrap(), 0);
    assert!(answers.inputs.is_empty());
    assert!(answers.text().contains("That folder has no API selector"));
    let requests = server.received_requests().await.unwrap();
    let root = requests
        .iter()
        .find(|request| request.url.path() == "/epost/v2/archives/letters")
        .unwrap();
    assert!(
        !root
            .url
            .query_pairs()
            .any(|(name, _)| name == "directory-id")
    );
    assert_eq!(requests.len(), 4);
    assert_read_only(&requests);
}

#[tokio::test]
async fn browse_next_page_stops_at_short_page_and_eof_returns_cleanly() {
    let server = MockServer::start().await;
    let temp = scratch();
    standard_auth(&server).await;
    for (offset, letters) in [
        ("0", json!([{ "id": "synthetic-first" }])),
        ("1", json!([])),
    ] {
        Mock::given(method("GET"))
            .and(path("/epost/v2/letters"))
            .and(query_param("offset", offset))
            .respond_with(ResponseTemplate::new(200).set_body_json(letters))
            .expect(1)
            .mount(&server)
            .await;
    }
    let cli = profile_cli(&server, temp.path(), &["--page-size", "1"]);
    let mut answers = Answers::new(&["1", "n", "n"]);
    assert_eq!(interactive::run(&cli, &mut answers).await.unwrap(), 0);
    assert!(answers.text().contains("No next page was indicated"));
    assert_eq!(server.received_requests().await.unwrap().len(), 4);
}

#[tokio::test]
async fn profile_must_still_belong_to_the_authenticated_account() {
    let server = MockServer::start().await;
    let temp = scratch();
    auth(
        &server,
        json!([{ "tenant_id": "synthetic-other-account", "company_id": 17 }]),
        0,
    )
    .await;
    let cli = profile_cli(&server, temp.path(), &[]);
    let mut answers = Answers::new(&[]);
    assert!(matches!(
        interactive::run(&cli, &mut answers).await,
        Err(Error::Config(_))
    ));
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    assert!(!temp.path().join("documents").exists());
}

#[tokio::test]
async fn invalid_guided_configuration_fails_before_credentials_or_requests() {
    let server = MockServer::start().await;
    let temp = scratch();
    for arguments in [
        vec!["setup", "--output", "relative-folder"],
        vec!["browse", "--output", "relative-folder"],
        vec!["--profile", "../invalid", "setup"],
    ] {
        let cli = cli(&server, temp.path(), &arguments);
        let mut answers = Answers::new(&[]);
        assert!(matches!(
            interactive::run(&cli, &mut answers).await,
            Err(Error::Config(_))
        ));
        assert_eq!(answers.credential_calls, 0);
    }
    assert!(server.received_requests().await.unwrap().is_empty());
    assert!(!temp.path().join("config.json").exists());
}

#[tokio::test]
async fn guided_sync_explains_inventory_failures_document_errors_and_cached_json() {
    let server = MockServer::start().await;
    let temp = scratch();
    standard_auth(&server).await;
    let cli = profile_cli(&server, temp.path(), &[]);
    let selected_mailbox = mailbox(&cli).unwrap();
    {
        let mut store = Store::open(
            &temp.path().join("documents"),
            &namespace(&cli, &selected_mailbox).unwrap(),
        )
        .unwrap();
        store
            .save(
                "synthetic-cached-json",
                "inbox",
                b"{\"invented\":\"message\"}\n",
                Some("application/json"),
            )
            .unwrap();
    }
    // A full page starts traversal, with duplicate entries to keep the fixture
    // small; the subsequent failure must be visible in the guided report.
    let mut letters = vec![json!({ "id": "synthetic-failed-content" })];
    letters.extend((0..47).map(|_| json!({ "id": "synthetic-cached-json" })));
    Mock::given(method("GET"))
        .and(path("/epost/v2/letters"))
        .and(query_param("offset", "0"))
        .respond_with(ResponseTemplate::new(200).set_body_json(letters))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/epost/v2/letters"))
        .and(query_param("offset", "48"))
        .respond_with(
            ResponseTemplate::new(500).set_body_string("synthetic-private-inventory-body"),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/epost/v2/letters/synthetic-failed-content/content"))
        .respond_with(ResponseTemplate::new(404).set_body_string("synthetic-private-content-body"))
        .expect(1)
        .mount(&server)
        .await;
    let mut answers = Answers::new(&["3", "yes", "q"]);
    assert_eq!(interactive::run(&cli, &mut answers).await.unwrap(), 2);
    let text = answers.text();
    assert!(text.contains("Sync needs attention"));
    assert!(text.contains("provider returned HTTP 500"));
    assert!(text.contains("provider returned HTTP 404"));
    assert!(text.contains("duplicate IDs observed across pages"));
    assert!(text.contains("synthetic-cached-json: already_saved (json)"));
    assert!(text.contains("synthetic-failed-content: failed"));
    assert!(!text.contains("synthetic-private-inventory-body"));
    assert!(!text.contains("synthetic-private-content-body"));
    assert_eq!(answers.credential_calls, 1);
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 5);
    assert!(
        !requests
            .iter()
            .any(|request| request.url.path() == "/epost/v2/letters/synthetic-cached-json/content")
    );
    assert_read_only(&requests);
    answers.assert_safe();
}

#[test]
fn provider_display_strings_cannot_emit_terminal_controls_or_hide_text_direction() {
    let clean =
        safe_line("Invented\u{1b}[2J\r\n\tTitle\u{202e}txt\u{2066}\u{200b}\u{feff}\u{061c}");
    assert!(!clean.chars().any(char::is_control));
    for unsafe_character in ['\u{202e}', '\u{2066}', '\u{200b}', '\u{feff}', '\u{061c}'] {
        assert!(!clean.contains(unsafe_character));
    }
    assert!(clean.contains("Invented"));
    let long = safe_line(&"é".repeat(121));
    assert_eq!(long.chars().count(), 121);
    assert!(long.ends_with('…'));
}
