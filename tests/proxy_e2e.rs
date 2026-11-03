//! End-to-end test: spawn the real binary and tunnel a connection through it.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command};
use std::thread;
use std::time::Duration;

/// Kills the child process when the test finishes (even on panic).
struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn connect_retry(addr: &str) -> TcpStream {
    for _ in 0..100 {
        if let Ok(stream) = TcpStream::connect(addr) {
            return stream;
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!("proxy did not start listening at {addr}");
}

/// Start a trivial TCP echo server; returns its address.
fn start_echo_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            thread::spawn(move || {
                let mut buf = [0u8; 1024];
                loop {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if stream.write_all(&buf[..n]).is_err() {
                                break;
                            }
                        }
                    }
                }
            });
        }
    });
    addr
}

#[test]
fn connect_tunnel_in_direct_mode() {
    let echo_addr = start_echo_server();
    let proxy_port = free_port();

    let child = Command::new(env!("CARGO_BIN_EXE_laliga-proxy"))
        .env("LALIGA_PROXY_DETECTOR", "static-direct")
        .env("LALIGA_PROXY_LISTEN", format!("127.0.0.1:{proxy_port}"))
        .env("LALIGA_PROXY_HEALTH_LISTEN", "127.0.0.1:0")
        .env("LALIGA_PROXY_LOG_FORMAT", "pretty")
        .spawn()
        .expect("failed to spawn laliga-proxy");
    let _guard = ChildGuard(child);

    let mut conn = connect_retry(&format!("127.0.0.1:{proxy_port}"));

    // Ask the proxy to tunnel to the echo server.
    let request = format!("CONNECT {echo_addr} HTTP/1.1\r\nHost: {echo_addr}\r\n\r\n");
    conn.write_all(request.as_bytes()).unwrap();

    let mut resp = [0u8; 128];
    let n = conn.read(&mut resp).unwrap();
    let resp = String::from_utf8_lossy(&resp[..n]);
    assert!(
        resp.contains("200"),
        "expected 200 established, got: {resp}"
    );

    // The tunnel is now transparent — the echo server should bounce our bytes.
    conn.write_all(b"hola").unwrap();
    let mut echoed = [0u8; 4];
    conn.read_exact(&mut echoed).unwrap();
    assert_eq!(&echoed, b"hola");
}

#[test]
fn chains_to_upstream_in_blocked_mode() {
    // A fake "upstream proxy" that just records that it received a CONNECT and
    // completes the tunnel to the echo server itself, proving we chained to it.
    let echo_addr = start_echo_server();
    let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
    let upstream_addr = upstream.local_addr().unwrap().to_string();

    thread::spawn(move || {
        for stream in upstream.incoming() {
            let Ok(mut client) = stream else { continue };
            let echo_addr = echo_addr.clone();
            thread::spawn(move || {
                // Read the CONNECT request head.
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while client.read(&mut byte).map(|n| n == 1).unwrap_or(false) {
                    head.push(byte[0]);
                    if head.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
                let head = String::from_utf8_lossy(&head);
                assert!(head.starts_with("CONNECT"), "upstream got: {head}");
                client
                    .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                    .unwrap();
                // Bridge the client to the real echo server.
                let mut server = TcpStream::connect(&echo_addr).unwrap();
                let mut c2 = client.try_clone().unwrap();
                let mut s2 = server.try_clone().unwrap();
                let t = thread::spawn(move || std::io::copy(&mut c2, &mut s2));
                let _ = std::io::copy(&mut server, &mut client);
                let _ = t.join();
            });
        }
    });

    let proxy_port = free_port();
    let child = Command::new(env!("CARGO_BIN_EXE_laliga-proxy"))
        .env("LALIGA_PROXY_DETECTOR", "static-blocked")
        .env("LALIGA_PROXY_LISTEN", format!("127.0.0.1:{proxy_port}"))
        .env("LALIGA_PROXY_HEALTH_LISTEN", "127.0.0.1:0")
        .env("LALIGA_PROXY_UPSTREAM", &upstream_addr)
        .env("LALIGA_PROXY_LOG_FORMAT", "pretty")
        .spawn()
        .expect("failed to spawn laliga-proxy");
    let _guard = ChildGuard(child);

    let mut conn = connect_retry(&format!("127.0.0.1:{proxy_port}"));
    let request = "CONNECT example.invalid:443 HTTP/1.1\r\nHost: example.invalid:443\r\n\r\n";
    conn.write_all(request.as_bytes()).unwrap();

    let mut resp = [0u8; 128];
    let n = conn.read(&mut resp).unwrap();
    let resp = String::from_utf8_lossy(&resp[..n]);
    assert!(
        resp.contains("200"),
        "expected 200 via upstream, got: {resp}"
    );

    conn.write_all(b"adios").unwrap();
    let mut echoed = [0u8; 5];
    conn.read_exact(&mut echoed).unwrap();
    assert_eq!(&echoed, b"adios");
}
