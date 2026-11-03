//! Tiny HTTP endpoint for Kubernetes probes and manual inspection.
//!
//! * `GET /healthz` (`/livez`, `/readyz`) → `200 {"status":"ok"}`
//! * `GET /status` (or `/`) → `200` with the current routing state and counters
//!
//! Kept dependency-free on purpose — a few lines of raw HTTP are plenty.

use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::config::Config;
use crate::state::SharedState;

pub async fn run(cfg: Arc<Config>, state: Arc<SharedState>) -> std::io::Result<()> {
    let listener = TcpListener::bind(&cfg.health_listen).await?;
    tracing::info!(event = "health_listening", addr = %cfg.health_listen, "health/status endpoint ready");

    loop {
        let (conn, _peer) = listener.accept().await?;
        let state = state.clone();
        tokio::spawn(async move {
            let _ = serve(conn, state).await;
        });
    }
}

async fn serve(mut conn: TcpStream, state: Arc<SharedState>) -> std::io::Result<()> {
    let mut buf = [0u8; 1024];
    let n = conn.read(&mut buf).await?;
    let request = String::from_utf8_lossy(&buf[..n]);
    let path = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/");

    let (status, body) = match path {
        "/healthz" | "/livez" | "/readyz" => ("200 OK", r#"{"status":"ok"}"#.to_string()),
        "/status" | "/" => {
            let snapshot = state.snapshot();
            let body = serde_json::to_string(&snapshot).unwrap_or_else(|_| "{}".to_string());
            ("200 OK", body)
        }
        _ => ("404 Not Found", r#"{"error":"not found"}"#.to_string()),
    };

    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n{body}",
        len = body.len(),
    );
    conn.write_all(response.as_bytes()).await?;
    conn.flush().await?;
    Ok(())
}
