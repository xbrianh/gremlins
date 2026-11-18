use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

use super::*;
use crate::test_support::EnvGuard;

async fn serve_responses(listener: TcpListener) {
    let (stream, _) = listener.accept().await.unwrap();
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    assert_eq!(line.trim_end(), "POST /responses HTTP/1.1");

    let mut headers = http::HeaderMap::new();
    loop {
        line.clear();
        assert_ne!(reader.read_line(&mut line).await.unwrap(), 0);
        if line == "\r\n" {
            break;
        }
        let (key, value) = line.split_once(':').unwrap();
        headers.append(
            http::HeaderName::from_bytes(key.as_bytes()).unwrap(),
            value.trim().parse().unwrap(),
        );
    }
    assert_eq!(headers[http::header::AUTHORIZATION], "Bearer env-token");
    let length = headers[http::header::CONTENT_LENGTH]
        .to_str()
        .unwrap()
        .parse::<usize>()
        .unwrap();
    let mut body = vec![0; length];
    reader.read_exact(&mut body).await.unwrap();
    let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(request["model"], "gpt-5.4");
    assert_eq!(request["stream"], true);

    let events = include_str!("done.sse");
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{events}",
        events.len()
    );
    reader
        .into_inner()
        .write_all(response.as_bytes())
        .await
        .unwrap();
}

#[tokio::test]
async fn run_uses_env_bearer_and_completes_from_responses_stream() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let root = tempfile::tempdir().unwrap();
    let mut env = EnvGuard::lock();
    env.set("GREMLINS_SANDBOX_ROOT", root.path());
    env.set(ACCESS_TOKEN_ENV, "env-token");
    let config_dir = root.path().join("config");
    std::fs::create_dir(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("providers.yaml"),
        "chatgpt:\n  token: file-token\n",
    )
    .unwrap();

    let backend =
        ChatGptBackend::build_with("gpt-5.4", &HashMap::new(), &IndexMap::new(), |config| {
            config.with_base_url(&base_url).client()
        })
        .unwrap();
    let prompt = "Say hello";
    let params = RunParams {
        prompt: prompt.into(),
        prepopulated_conversation: crate::conversation::new_blank_conversation(prompt, false, None),
        cwd: Some(root.path().to_owned()),
        ..RunParams::empty()
    };
    let (_, completed) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(
            serve_responses(listener),
            backend.run(params, InteractiveSession::unattached())
        )
    })
    .await
    .expect("timed out waiting for Responses run");
    let completed = completed.unwrap();
    assert_eq!(completed.exit_code, 0);
    assert_eq!(completed.text_result.as_deref(), Some("Hello from ChatGPT"));
}
