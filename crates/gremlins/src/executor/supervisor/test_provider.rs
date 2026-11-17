use serde_json::Value;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

/// Read the model request but withhold the response. Tests can now interrupt
/// the stream deterministically, instead of racing a scripted completion.
pub(super) async fn pending_request(provider: &TcpListener) -> (Value, TcpStream) {
    tokio::time::timeout(Duration::from_secs(10), async {
        let (stream, _) = provider.accept().await.unwrap();
        let mut reader = BufReader::new(stream);
        let mut length = None;
        loop {
            let mut header = String::new();
            assert!(reader.read_line(&mut header).await.unwrap() > 0);
            if header == "\r\n" {
                break;
            }
            if let Some((name, value)) = header.split_once(':') {
                if name.eq_ignore_ascii_case("content-length") {
                    length = Some(value.trim().parse::<usize>().unwrap());
                }
            }
        }
        let mut body = vec![0; length.expect("missing request body length")];
        reader.read_exact(&mut body).await.unwrap();
        (serde_json::from_slice(&body).unwrap(), reader.into_inner())
    })
    .await
    .expect("expected a model request")
}
