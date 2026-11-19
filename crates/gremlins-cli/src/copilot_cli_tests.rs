use super::{run_cli, Cli};
use clap::Parser;
use serde_json::json;
use std::{fs, io::Write, os::fd::AsRawFd, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    process::Command,
};

const STATIC: &str = "static-secret-sentinel";
const GITHUB: &str = "github-secret-sentinel";
const SESSION: &str = "session-secret-sentinel";
const REFRESH: &str = "refresh-secret-sentinel";
const CLIENT: &str = "client-secret-sentinel";
const BODY: &str = "provider-body-secret-sentinel";
const DESCRIPTION: &str = "oauth-description-secret-sentinel";

// Run only in a fresh child so even process::exit in the CLI dispatcher is tested.
#[tokio::test]
#[ignore = "child entry point for Copilot acceptance tests"]
async fn child() {
    let Ok(base) = std::env::var("COPILOT_CLI_TEST_BASE") else {
        return;
    };
    let _routes = gremlins::clients::with_copilot_test_endpoints(
        &format!("{base}/exchange"),
        &format!("{base}/refresh"),
        &format!("{base}/default"),
    );
    // libtest writes its own banner to stdout. Capture only what the CLI writes
    // after this point, without changing the production output/error handlers.
    let file = fs::File::create("command.stdout").unwrap();
    std::io::stdout().flush().unwrap();
    // SAFETY: both descriptors are valid; this child runs only this test.
    assert_ne!(
        unsafe { libc::dup2(file.as_raw_fd(), libc::STDOUT_FILENO) },
        -1
    );
    run_cli(Cli::parse_from(["gremlins", "list-models", "copilot"])).await;
    std::io::stdout().flush().unwrap();
    std::process::exit(0);
}

#[derive(Clone, Copy)]
enum Case {
    Static,
    Assigned,
    Fallback,
    Missing,
    Refresh(u16),
    Exchange,
    Catalog(u16),
    Transport,
    Invalid,
}

#[derive(Debug)]
struct Request {
    line: String,
    headers: String,
    body: String,
}

async fn read_request(stream: &mut TcpStream) -> Request {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    let mut headers = String::new();
    let mut size = 0;
    loop {
        let mut header = String::new();
        assert_ne!(reader.read_line(&mut header).await.unwrap(), 0);
        if header == "\r\n" {
            break;
        }
        let (name, value) = header.split_once(':').unwrap();
        if name.eq_ignore_ascii_case("content-length") {
            size = value.trim().parse().unwrap();
        }
        headers.push_str(&format!(
            "{}: {}\n",
            name.to_ascii_lowercase(),
            value.trim()
        ));
    }
    let mut body = vec![0; size];
    reader.read_exact(&mut body).await.unwrap();
    Request {
        line: line.trim().into(),
        headers,
        body: String::from_utf8(body).unwrap(),
    }
}

fn reply(case: Case, request: &Request, base: &str) -> Option<(u16, String)> {
    match request.line.as_str() {
        "POST /refresh HTTP/1.1" => {
            let Case::Refresh(status) = case else { panic!("unexpected app refresh") };
            Some((status, json!({"error":"invalid_grant", "error_description":DESCRIPTION}).to_string()))
        }
        "GET /exchange HTTP/1.1" => {
            if matches!(case, Case::Exchange) { return Some((403, BODY.into())); }
            let mut record = json!({"token": SESSION, "expires_at": 4102444800_i64});
            if matches!(case, Case::Assigned) {record["endpoints"] = json!({"api": format!("{base}/assigned")});}
            Some((200, record.to_string()))
        }
        "GET /assigned/models HTTP/1.1" | "GET /default/models HTTP/1.1" => match case {
            Case::Catalog(code) => Some((code, BODY.into())),
            Case::Invalid => Some((200, BODY.into())),
            Case::Transport => None, // close the connection before sending a response
            Case::Fallback => Some((200, json!({"data":[{"id":"embed", "capabilities":{"type":"embedding"}}]}).to_string())),
            _ => Some((200, json!({"data":[
                {"id":"z", "name":"First display name", "capabilities":{"type":"chat"}},
                {"id":"a", "name":"Second display name", "capabilities":{"type":"chat"}},
                {"id":"z", "capabilities":{"type":"chat"}}, {"id":"  ", "capabilities":{"type":"chat"}},
                {"id":"embed", "capabilities":{"type":"embedding"}}, {"id":"missing"}, {"id":"null", "capabilities":null}
            ]}).to_string())),
        },
        other => panic!("unexpected request: {other}"),
    }
}

async fn run(case: Case) -> (bool, String, String, Vec<Request>) {
    let root = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args([
        "--ignored",
        "--exact",
        "copilot_cli_tests::child",
        "--nocapture",
    ])
    .env_clear()
    .current_dir(root.path())
    .env("HOME", root.path())
    .env("GREMLINS_SANDBOX_ROOT", root.path())
    .env("COPILOT_CLI_TEST_BASE", &base)
    // Fail closed if a regression routes a request to a real provider.
    .env("HTTPS_PROXY", &base)
    .env("HTTP_PROXY", &base)
    .env("NO_PROXY", "127.0.0.1")
    .kill_on_drop(true)
    .stdout(Stdio::null())
    .stderr(Stdio::piped());
    match case {
        Case::Missing => (),
        Case::Refresh(_) => {
            let dir = root.path().join("config");
            fs::create_dir(&dir).unwrap();
            fs::write(
                dir.join("providers.yaml"),
                format!("copilot:\n  client-id: {CLIENT}\n  refresh-token: {REFRESH}\n"),
            )
            .unwrap();
        }
        Case::Assigned | Case::Fallback | Case::Exchange => {
            cmd.env("GITHUB_TOKEN", GITHUB);
        }
        _ => {
            cmd.env("GITHUB_COPILOT_API_KEY", STATIC)
                .env("GITHUB_TOKEN", GITHUB);
        }
    }
    let child = cmd.spawn().unwrap().wait_with_output();
    tokio::pin!(child);
    let mut requests = Vec::new();
    let output = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            tokio::select! {
                result = &mut child => break result.unwrap(),
                conn = listener.accept() => {
                    let (mut stream, _) = conn.unwrap();
                    let request = read_request(&mut stream).await;
                    if let Some((status, body)) = reply(case, &request, &base) {
                        stream.write_all(format!("HTTP/1.1 {status} Mock\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}", body.len(), body).as_bytes()).await.unwrap();
                    }
                    requests.push(request);
                }
            }
        }
    }).await.expect("CLI timed out");
    let stderr = String::from_utf8(output.stderr).unwrap();
    let stdout = fs::read_to_string(root.path().join("command.stdout"))
        .unwrap_or_else(|e| panic!("child did not start CLI: {e}; {stderr}"));
    for secret in [STATIC, GITHUB, SESSION, REFRESH, CLIENT, BODY, DESCRIPTION] {
        assert!(
            !stdout.contains(secret) && !stderr.contains(secret),
            "secret in CLI output"
        );
    }
    (output.status.success(), stdout, stderr, requests)
}

#[tokio::test]
async fn renders_chat_uris_in_order_with_only_the_selected_bearer() {
    for (case, path, bearer, expected) in [
        (
            Case::Static,
            "default",
            STATIC,
            "model://copilot/z\nmodel://copilot/a\n",
        ),
        (
            Case::Assigned,
            "assigned",
            SESSION,
            "model://copilot/z\nmodel://copilot/a\n",
        ),
        (Case::Fallback, "default", SESSION, ""),
    ] {
        let (ok, stdout, stderr, requests) = run(case).await;
        assert!(ok, "{stderr}");
        assert_eq!(stdout, expected);
        assert!(stderr.is_empty(), "{stderr}");
        let (catalog, exchange) = requests.split_last().unwrap();
        if matches!(case, Case::Static) {
            assert!(exchange.is_empty());
        } else {
            assert_eq!(exchange.len(), 1);
            assert_eq!(exchange[0].line, "GET /exchange HTTP/1.1");
            assert!(exchange[0]
                .headers
                .contains(&format!("authorization: token {GITHUB}\n")));
        }
        assert_eq!(catalog.line, format!("GET /{path}/models HTTP/1.1"));
        assert!(catalog
            .headers
            .contains(&format!("authorization: Bearer {bearer}\n")));
        assert!(!catalog.headers.contains(GITHUB) && !catalog.body.contains(GITHUB));
        assert!(catalog.headers.contains("editor-version: vscode/"));
        assert!(catalog
            .headers
            .contains("editor-plugin-version: copilot-chat/"));
    }
}

#[tokio::test]
async fn failures_exit_nonzero_with_only_local_diagnostics() {
    for (case, diagnostic) in [
        (Case::Missing, &["credential", "GITHUB_COPILOT_API_KEY"][..]),
        (Case::Refresh(400), &["refresh", "400", "refresh-token"]),
        // OAuth can reject a refresh in a successful HTTP envelope.
        (Case::Refresh(200), &["refresh", "200", "refresh-token"]),
        (Case::Exchange, &["exchange", "403", "GitHub"]),
        (Case::Catalog(401), &["catalog", "401", "credential"]),
        (Case::Catalog(403), &["catalog", "403", "credential"]),
        (Case::Transport, &["catalog", "network"]),
        (Case::Invalid, &["catalog", "decode"]),
    ] {
        let (ok, stdout, stderr, requests) = run(case).await;
        assert!(!ok);
        assert_eq!(stdout, "");
        for part in diagnostic {
            assert!(stderr.contains(part), "{stderr}");
        }
        match case {
            Case::Missing => assert!(requests.is_empty()),
            Case::Refresh(_) => {
                assert_eq!(requests.len(), 1);
                assert_eq!(requests[0].line, "POST /refresh HTTP/1.1");
                assert!(requests[0]
                    .body
                    .contains(&format!("refresh_token={REFRESH}")));
                assert!(requests[0].body.contains(&format!("client_id={CLIENT}")));
            }
            Case::Exchange => {
                assert_eq!(requests.len(), 1);
                assert_eq!(requests[0].line, "GET /exchange HTTP/1.1");
            }
            _ => {
                assert!(!requests.is_empty());
                assert!(requests
                    .iter()
                    .all(|r| r.line == "GET /default/models HTTP/1.1"));
            }
        }
    }
}
