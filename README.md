# Proxy Systems

Chain of proxy servers written in Rust for routing and load-balancing HTTP requests.

```
Client -> [Intermediate Proxy :3000] -> [Simple Proxy / Simple IPv6 Proxy / Tor Proxy / Cloudflare Workers / HTTP proxy / SOCKS5 proxy]
```

## Components

### simple-proxy

Basic reverse proxy. Receives the target URL as a base64-encoded `X-Target` header, forwards the request, and streams the response back with CORS headers. Handles redirects (relative, absolute, protocol-relative).

**Port:** `8080` (env `PORT`)

### simple-ipv6-proxy

IPv6-only variant of `simple-proxy`. Resolves the target via AAAA records only and connects over IPv6. If the target has no IPv6 address (or all IPv6 attempts fail), it returns `502` so `intermediate-proxy` fails over to the next upstream.

- **AAAA-only resolution** — IPv4 records are dropped; `502` if the target has no IPv6.
- **Source address rotation** — when `IPV6_SUBNET` is set, each attempt binds a fresh random IPv6 from that subnet. Useful on hosts with a routed `/64` (Hetzner, OVH, etc.): each outgoing request appears from a different source IP, which beats naive per-IP blocks.
- **Ban-aware retry** — if the target replies with a status in `RETRY_STATUS_CODES` (default `403,429`) or the connection fails, retries up to `MAX_ATTEMPTS` times; each attempt picks a new random source address and rotates through resolved AAAA records. If all attempts fail, returns `502` so the caller can fail over.
- **SSRF guard** — literal internal / loopback / link-local / ULA / CGN / documentation IPs are rejected outright; resolved AAAA addresses are filtered the same way.
- **IPV6_FREEBIND** — set on the outgoing socket so the kernel accepts binding to any address in a routed prefix, even if it isn't configured on a local interface.
- **Streaming response body** — via hyper + rustls, same as `simple-proxy`.

**Port:** `8080` (env `PORT`)

| Env | Description | Default |
|-----|-------------|---------|
| `IPV6_SUBNET` | CIDR of a routed IPv6 prefix to rotate source addresses over (e.g. `2001:db8:abcd::/48`). If unset, uses the default outgoing IPv6. | — |
| `CONNECT_TIMEOUT_MS` | TCP connect timeout per attempt | `5000` |
| `REQUEST_TIMEOUT_MS` | HTTP request timeout per attempt | `30000` |
| `MAX_ATTEMPTS` | Total attempts per request (retries with fresh random source on connect error or ban-status) | `3` |
| `RETRY_STATUS_CODES` | Upstream status codes that trigger a retry | `403,429` |

For subnet rotation to work:
1. The prefix must be routed to this host (the hoster's routing, not just `ip addr add`).
2. On Linux, either set `net.ipv6.ip_nonlocal_bind=1` or run with `CAP_NET_ADMIN`; the proxy also sets `IPV6_FREEBIND` per-socket for reliability.
3. Container must have IPv6 enabled (Docker: `--sysctl net.ipv6.conf.all.disable_ipv6=0`, plus network configured with an IPv6 subnet).

### intermediate-proxy

Smart router with automatic failover, latency-aware tier ordering, and lock-free hot path.

- **Tiered priority queue** — each upstream is classified as `healthy` / `slow` / `failed` from runtime stats (EWMA latency, consecutive failures). Ordering is `(regular ⟶ reserve) × (healthy ⟶ slow ⟶ failed)`, so bad proxies drift to a "not recommended" zone and reserves always stay at the end.
- **Lock-free snapshots** — reads use `ArcSwap`; stats are plain atomics. A background task re-sorts the queue when a tier transition is detected (debounced by `RESORT_INTERVAL_MS`).
- **Outcome classification** — `421/429/500/502/503` → soft-fail (penalized latency), `403` → skip without penalty (target block), network error / timeout → hard-fail.
- **Stream recovery** — on mid-stream failure for GET requests with cache/media headers, resumes from the next un-tried proxy (using the live snapshot) via `Range` header.
- **HTTP/1 only** — required for Cloudflare `workers.dev` upstreams.
- **Upstream types** determined by URL scheme:
  - `http://` / `https://` — endpoint (forward with `X-Target` header)
  - `socks5://host:port` — SOCKS5 proxy (request goes directly to target)
  - `forward://host:port` — HTTP forward proxy

**Port:** `3000` (env `PORT`)

| Env | Description | Default |
|-----|-------------|---------|
| `PROXY_URL` | Comma-separated list of upstream proxies | `http://localhost:8080` |
| `RESERVE_PROXY_URL` | Comma-separated list of reserve proxies | — |
| `SLOW_THRESHOLD_MS` | EWMA latency above this demotes a proxy to the `slow` tier | `3000` |
| `UPSTREAM_TIMEOUT_MS` | Per-attempt time-to-first-byte timeout | `10000` |
| `RESORT_INTERVAL_MS` | Max delay between queue re-sorts when stats change | `2000` |

`GET /health` returns per-proxy tier, average / last latency, success/error counts, consecutive failures, last error reason, plus tier totals.

#### Built-in TLS (Let's Encrypt)

The intermediate-proxy can terminate TLS directly — no nginx needed. Certificates are issued and renewed automatically via ACME (TLS-ALPN-01 challenge on :443). When TLS is enabled the proxy binds **:80** (plain HTTP) and **:443** (HTTPS with SNI for all configured domains); `PORT` is ignored.

| Env | Description | Default |
|-----|-------------|---------|
| `TLS_ENABLED` | Enable HTTPS mode (binds :80 + :443, ignores `PORT`) | `false` |
| `DOMAINS` | Comma-separated list of domain names served on :443 | — |
| `ACME_EMAIL` | Contact email for Let's Encrypt | `admin@{first domain}` |
| `ACME_CACHE_DIR` | Persistent cert cache directory (mount a volume) | `/var/cache/acme` |
| `ACME_STAGING` | Use Let's Encrypt staging directory for testing | `false` |

Example:

```
TLS_ENABLED=true
DOMAINS=proxy.example.com,api.example.com
ACME_EMAIL=admin@example.com
```

The cache directory must survive restarts (mount a Docker volume to `/var/cache/acme`), otherwise the proxy will hit Let's Encrypt rate limits on every restart. Start with `ACME_STAGING=true` when setting up a new deployment. DNS for every entry in `DOMAINS` must resolve to this host, and port 443 must be reachable from the internet for ACME validation.

### tor-proxy

Routes requests through Tor via SOCKS5 with automatic circuit rotation.

- Round-robin across multiple Tor nodes with cooldown
- Automatic `SIGNAL NEWNYM` after consecutive error threshold
- Scheduled periodic rotation
- HTTP-over-SOCKS5 tunnel with TLS upgrade via hyper

**Port:** `8080` (env `PORT`)

| Env | Description | Default |
|-----|-------------|---------|
| `TOR_NODES` | `host:socksPort:controlPort,...` | `tor-node-1:9050:9051` |
| `TOR_CONTROL_PASSWORD` | Control port password | `torcontrol` |
| `ROTATION_INTERVAL_MS` | Scheduled rotation interval | `3600000` |
| `ERROR_THRESHOLD` | Consecutive errors before NEWNYM | `3` |
| `NEWNYM_COOLDOWN_MS` | Min time between NEWNYMs per node | `15000` |
| `SOCKS_TIMEOUT_MS` | SOCKS5 connect timeout | `15000` |
| `REQUEST_TIMEOUT_MS` | HTTP request timeout | `30000` |

## Health checks

All proxies expose `GET /health` returning JSON with current status and stats.

## Logging

All services share the same logging setup. Levels are resolved in this order:

1. `RUST_LOG` — full [EnvFilter](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html) directive, e.g. `RUST_LOG=intermediate_proxy=debug,warn`.
2. `LOG_LEVEL` — simple level name: `trace` / `debug` / `info` / `warn` / `error`.
3. Built-in default (`info`).

For production set `LOG_LEVEL=warn` (or `error`). Per-request / per-upstream-attempt traces live at `debug`, so they vanish under `warn` and don't cost CPU. Tier changes, queue re-sorts, recovery events, and real network errors stay at `info` / `warn`.

## Build

```bash
cargo build --release
```

Binaries: `target/release/simple-proxy`, `target/release/simple-ipv6-proxy`, `target/release/intermediate-proxy`, `target/release/tor-proxy`

## Docker

```bash
docker build -f Dockerfile.simple-proxy -t simple-proxy .
docker build -f Dockerfile.simple-ipv6-proxy -t simple-ipv6-proxy .
docker build -f Dockerfile.intermediate-proxy -t intermediate-proxy .
docker build -f Dockerfile.tor-proxy -t tor-proxy .
```

## Release

Push to `main` with `!release: patch`, `!release: minor`, or `!release: major` in the commit message. GitHub Action builds and pushes images to GHCR:

```
ghcr.io/zxcloli666/proxy-systems/simple-proxy:latest
ghcr.io/zxcloli666/proxy-systems/simple-ipv6-proxy:latest
ghcr.io/zxcloli666/proxy-systems/intermediate-proxy:latest
ghcr.io/zxcloli666/proxy-systems/tor-proxy:latest
```
