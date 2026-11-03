# laliga-proxy

> A conditional forward proxy that reroutes your traffic through a VPN **only while LaLiga is blocking Cloudflare IPs**, and goes straight the rest of the time.

During football broadcasts, Spanish ISPs block ranges of Cloudflare IPs to fight
piracy. The collateral damage is huge: perfectly legitimate services that happen
to sit behind Cloudflare (metadata APIs, indexers, container registries, random
websites…) become unreachable from Spain for a few hours.

If you run self-hosted apps (Sonarr, Radarr, Prowlarr, …), the usual workaround
is to point each app at a VPN proxy — but then **all** your traffic goes through
the VPN, which is slower, rate-limited and sometimes IP-banned by the very
services you want to reach.

`laliga-proxy` fixes that. Your apps point at it **once** and never change their
settings again:

```
             ┌──────────────── no block ───────────────►  direct to target
app ──proxy──┤
             └──────────────── block active ───────────►  upstream proxy (VPN)
```

* **No block?** Transparent passthrough — a plain TCP relay, negligible overhead.
* **Block active?** It chains to an upstream HTTP proxy (e.g. [gluetun](https://github.com/qdm12/gluetun)) so the request egresses through the VPN.

It decides which mode to use by watching [hayahora.futbol](https://hayahora.futbol/),
a community project that monitors the blocked Cloudflare IPs in real time.

## How detection works

hayahora.futbol doesn't know whether a match is on — it continuously probes a set
of Cloudflare IPs per ISP and records when they go down. A block manifests as a
large fraction of those IPs becoming unreachable.

`laliga-proxy` polls `https://hayahora.futbol/estado/data.json`, computes the
fraction of monitored IPs whose latest state is *down*, and compares it to a
threshold. To avoid flapping, the decision is debounced with **asymmetric
hysteresis**:

* switch **to the VPN quickly** (default: a single positive reading) — being late
  here means failed requests;
* switch **back to direct conservatively** (default: 3 consecutive clear
  readings) — being late here is harmless, the VPN path still works.

A connection keeps whichever route it was assigned when it was accepted. Apps
like the *arr suite open many short-lived connections, so a state change is
picked up almost immediately.

## Quick start

### Docker

```sh
docker run -d --name laliga-proxy \
  -p 8888:8888 -p 9090:9090 \
  -e LALIGA_PROXY_UPSTREAM=gluetun:8888 \
  ghcr.io/danielramosacosta/laliga-proxy:latest
```

Then set the HTTP proxy of each app to `http://<host>:8888`.

Images are published for `linux/amd64` and `linux/arm64` (works on a Raspberry Pi).

### Try it locally

```sh
# Always passthrough (no VPN), useful to sanity-check the proxy:
LALIGA_PROXY_DETECTOR=static-direct cargo run

# In another terminal:
curl -x http://127.0.0.1:8888 https://api.themoviedb.org/3/configuration
curl http://127.0.0.1:9090/status | jq
```

## Configuration

Everything is configured through environment variables:

| Variable | Default | Description |
| --- | --- | --- |
| `LALIGA_PROXY_LISTEN` | `0.0.0.0:8888` | Address the forward proxy listens on. |
| `LALIGA_PROXY_HEALTH_LISTEN` | `0.0.0.0:9090` | Address of the health/status endpoint. |
| `LALIGA_PROXY_UPSTREAM` | `gluetun.system.svc.cluster.local:8888` | Upstream HTTP proxy used while a block is active. |
| `LALIGA_PROXY_DETECTOR` | `hayahora-json` | `hayahora-json`, `static-direct` or `static-blocked`. |
| `LALIGA_PROXY_HAYAHORA_URL` | `https://hayahora.futbol/estado/data.json` | Signal source for the `hayahora-json` detector. |
| `LALIGA_PROXY_POLL_INTERVAL` | `30` | Seconds between polls. |
| `LALIGA_PROXY_BLOCK_THRESHOLD` | `0.5` | Fraction of IPs down (`0.0`–`1.0`) to consider a block active. |
| `LALIGA_PROXY_BLOCK_CONFIRM` | `1` | Consecutive positive readings required to switch to the VPN. |
| `LALIGA_PROXY_UNBLOCK_CONFIRM` | `3` | Consecutive clear readings required to switch back to direct. |
| `LALIGA_PROXY_FAIL_MODE` | `keep` | What to assume if the signal is unreachable: `keep`, `direct` or `blocked`. |
| `LALIGA_PROXY_LOG_FORMAT` | `json` | `json` (structured, for Loki/ELK) or `pretty` (human readable). |
| `LALIGA_PROXY_LOG_LEVEL` | `info` | Log filter, e.g. `info` or `laliga_proxy=debug`. Falls back to `RUST_LOG`. |

## Endpoints

The health server (`:9090`) speaks plain HTTP:

* `GET /healthz` (`/livez`, `/readyz`) → `200 {"status":"ok"}` — for Kubernetes probes.
* `GET /status` → current routing state, counters and last poll result:

```json
{
  "blocked": false,
  "route": "direct",
  "total_connections": 128,
  "direct_connections": 128,
  "upstream_connections": 0,
  "poll": {
    "last_poll_ok": true,
    "last_blocked_ratio": 0.02,
    "last_update": "2026-11-03 19:28:05Z",
    "last_error": null
  }
}
```

## Logging

Logs are structured JSON by default, one event per line, with a stable `event`
field for easy querying. Useful events: `startup`, `state_change`, `poll_ok`,
`poll_error`, `route`, `upstream_connect_error`. Example (pretty-printed):

```json
{"timestamp":"...","level":"INFO","event":"state_change","blocked":true,"route":"upstream","blocked_ratio":0.78,"message":"¡hay futbol! routing via upstream (gluetun)"}
```

In Loki you can, for instance, alert on `event="state_change"` or graph
`blocked_ratio` over time.

## Notes & limitations

* Primarily an HTTPS `CONNECT` tunnel proxy (the common case). Plain-HTTP
  absolute-form requests are forwarded too, one origin per connection.
* While a block is active the client stream is relayed verbatim to the upstream
  proxy, so whatever auth/behaviour the upstream supports is preserved.
* The upstream must be an **HTTP** proxy (gluetun's `HTTPPROXY=on`, Squid,
  tinyproxy, …), not SOCKS.

## Development

```sh
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test            # unit tests + an end-to-end test that spawns the binary
```

## License

[MIT](./LICENSE)
