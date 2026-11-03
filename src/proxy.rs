//! The forward proxy itself.
//!
//! Clients (the *arr apps) always point at this proxy. For each new connection
//! we read the current routing decision once, at accept time:
//!
//! * **blocked** → dial the upstream HTTP proxy (gluetun) and relay the raw
//!   client stream verbatim. The client is already speaking the HTTP-proxy
//!   protocol and gluetun understands it, so both `CONNECT` tunnels and
//!   absolute-form requests just work with a transparent byte pipe.
//! * **direct** → we act as the proxy ourselves: terminate `CONNECT` and dial
//!   the target directly, or forward an absolute-form HTTP request as origin
//!   form.
//!
//! A connection keeps whatever route it was assigned at accept time for its
//! whole lifetime; the *arr apps open many short-lived connections, so new
//! state is picked up almost immediately.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::config::Config;
use crate::state::SharedState;

const MAX_HEAD: usize = 64 * 1024;

pub async fn run(cfg: Arc<Config>, state: Arc<SharedState>) -> anyhow::Result<()> {
    let listener = TcpListener::bind(&cfg.listen).await?;
    tracing::info!(event = "listening", addr = %cfg.listen, "proxy accepting connections");

    loop {
        let (client, peer) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                tracing::warn!(event = "accept_error", error = %e, "accept failed");
                continue;
            }
        };
        let cfg = cfg.clone();
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(client, peer, cfg, state).await {
                tracing::debug!(event = "conn_error", peer = %peer, error = %e, "connection closed with error");
            }
        });
    }
}

async fn handle(
    mut client: TcpStream,
    peer: SocketAddr,
    cfg: Arc<Config>,
    state: Arc<SharedState>,
) -> std::io::Result<()> {
    state.inc_total();
    if state.is_blocked() {
        route_upstream(&mut client, peer, &cfg, &state).await
    } else {
        route_direct(&mut client, peer, &state).await
    }
}

/// Relay the raw proxy stream to the configured upstream HTTP proxy.
async fn route_upstream(
    client: &mut TcpStream,
    peer: SocketAddr,
    cfg: &Config,
    state: &SharedState,
) -> std::io::Result<()> {
    state.inc_upstream();
    let mut upstream = match TcpStream::connect(&cfg.upstream).await {
        Ok(stream) => stream,
        Err(e) => {
            tracing::error!(
                event = "upstream_connect_error",
                upstream = %cfg.upstream,
                peer = %peer,
                error = %e,
                "cannot reach upstream proxy"
            );
            return Err(e);
        }
    };
    tracing::debug!(
        event = "route",
        route = "upstream",
        peer = %peer,
        upstream = %cfg.upstream,
        "relaying raw proxy stream to upstream"
    );
    tokio::io::copy_bidirectional(client, &mut upstream).await?;
    Ok(())
}

/// Act as the proxy ourselves and connect to the target directly.
async fn route_direct(
    client: &mut TcpStream,
    peer: SocketAddr,
    state: &SharedState,
) -> std::io::Result<()> {
    state.inc_direct();
    let head = read_head(client).await?;
    if head.is_empty() {
        return Ok(());
    }

    let head_str = std::str::from_utf8(&head).map_err(|_| invalid("non-utf8 request head"))?;
    let request_line = head_str.lines().next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");

    if method.eq_ignore_ascii_case("CONNECT") {
        let addr = normalize_authority(target, 443);
        let mut server = TcpStream::connect(&addr).await?;
        client
            .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
            .await?;
        tracing::debug!(
            event = "route",
            route = "direct",
            method = "CONNECT",
            peer = %peer,
            target = %addr,
            "tunnel established"
        );
        tokio::io::copy_bidirectional(client, &mut server).await?;
        Ok(())
    } else if let Some((authority, path)) = split_absolute_uri(target) {
        let addr = normalize_authority(&authority, 80);
        let mut server = TcpStream::connect(&addr).await?;
        // Rewrite the request line from absolute form to origin form.
        let origin_line = format!(
            "{} {} HTTP/1.1\r\n",
            method,
            if path.is_empty() { "/" } else { &path }
        );
        server.write_all(origin_line.as_bytes()).await?;
        // Forward the remaining headers (and any body bytes already buffered).
        let first_line_end = head_str.find("\r\n").map(|p| p + 2).unwrap_or(head.len());
        server.write_all(&head[first_line_end..]).await?;
        tracing::debug!(
            event = "route",
            route = "direct",
            method = %method,
            peer = %peer,
            target = %addr,
            "forwarding plain HTTP request"
        );
        tokio::io::copy_bidirectional(client, &mut server).await?;
        Ok(())
    } else {
        tracing::debug!(
            event = "bad_request",
            peer = %peer,
            request_line = %request_line,
            "unsupported proxy request"
        );
        let _ = client.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n").await;
        Ok(())
    }
}

/// Read bytes until the end of the HTTP request head (`\r\n\r\n`). The returned
/// buffer may contain a few body bytes past the head if they arrived together.
async fn read_head(client: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(1024);
    let mut tmp = [0u8; 4096];
    loop {
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            return Ok(buf);
        }
        if buf.len() > MAX_HEAD {
            return Err(invalid("request head too large"));
        }
        let n = client.read(&mut tmp).await?;
        if n == 0 {
            // EOF before a complete head; let the caller treat it as empty.
            return Ok(buf);
        }
        buf.extend_from_slice(&tmp[..n]);
    }
}

fn invalid(msg: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, msg.to_string())
}

/// Ensure an authority has an explicit port, appending `default_port` if not.
fn normalize_authority(authority: &str, default_port: u16) -> String {
    if authority.starts_with('[') {
        // IPv6 literal, e.g. [::1] or [::1]:443 — only add a port if missing.
        if authority.rsplit(']').next().unwrap_or("").contains(':') {
            authority.to_string()
        } else {
            format!("{authority}:{default_port}")
        }
    } else if authority.contains(':') {
        authority.to_string()
    } else {
        format!("{authority}:{default_port}")
    }
}

/// Split `http://host:port/path?x` into (`host:port`, `/path?x`).
fn split_absolute_uri(target: &str) -> Option<(String, String)> {
    let rest = target
        .strip_prefix("http://")
        .or_else(|| target.strip_prefix("https://"))?;
    match rest.find('/') {
        Some(idx) => Some((rest[..idx].to_string(), rest[idx..].to_string())),
        None => Some((rest.to_string(), "/".to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authority_gets_default_port() {
        assert_eq!(normalize_authority("example.com", 443), "example.com:443");
        assert_eq!(
            normalize_authority("example.com:8080", 443),
            "example.com:8080"
        );
    }

    #[test]
    fn authority_ipv6() {
        assert_eq!(normalize_authority("[::1]", 443), "[::1]:443");
        assert_eq!(normalize_authority("[::1]:8080", 443), "[::1]:8080");
    }

    #[test]
    fn splits_absolute_uri() {
        assert_eq!(
            split_absolute_uri("http://example.com/a/b?c=1"),
            Some(("example.com".to_string(), "/a/b?c=1".to_string()))
        );
        assert_eq!(
            split_absolute_uri("http://example.com:81"),
            Some(("example.com:81".to_string(), "/".to_string()))
        );
        assert_eq!(split_absolute_uri("/just/a/path"), None);
    }
}
