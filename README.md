# Proxy Systems

Chain of proxy servers written in Rust for routing and load-balancing HTTP requests.

```
Client -> [Intermediate Proxy :3000] -> [Simple Proxy / Tor Proxy / Cloudflare Workers / HTTP proxy / SOCKS5 proxy]
```

## Components

### simple-proxy

Basic reverse proxy. Receives the target URL as a base64-encoded `X-Target` header, forwards the request, and streams the response back with CORS headers. Handles redirects (relative, absolute, protocol-relative).

**Port:** `8080` (env `PORT`)

### intermediate-proxy

Smart router with automatic failover across multiple upstream proxies.

- **Priority queue** with versioned reordering — only the first concurrent request to fail on a proxy modifies the global order
- **Reserve proxies** — emergency backups that always stay at the end of the queue
- **Stream recovery** — on mid-stream failure for GET requests with cache/media headers, resumes from the next proxy using `Range` header
- **Upstream types** determined by URL scheme:
  - `http://` / `https://` — endpoint (forward with `X-Target` header)
  - `socks5://host:port` — SOCKS5 proxy (request goes directly to target)
  - `forward://host:port` — HTTP forward proxy

**Port:** `3000` (env `PORT`)

| Env | Description | Default |
|-----|-------------|---------|
| `PROXY_URL` | Comma-separated list of upstream proxies | `http://localhost:8080` |
| `RESERVE_PROXY_URL` | Comma-separated list of reserve proxies | — |

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

## Build

```bash
cargo build --release
```

Binaries: `target/release/simple-proxy`, `target/release/intermediate-proxy`, `target/release/tor-proxy`

## Docker

```bash
docker build -f Dockerfile.simple-proxy -t simple-proxy .
docker build -f Dockerfile.intermediate-proxy -t intermediate-proxy .
docker build -f Dockerfile.tor-proxy -t tor-proxy .
```

## Release

Push to `main` with `!release: patch`, `!release: minor`, or `!release: major` in the commit message. GitHub Action builds and pushes images to GHCR:

```
ghcr.io/<owner>/proxy-systems/simple-proxy:<version>
ghcr.io/<owner>/proxy-systems/intermediate-proxy:<version>
ghcr.io/<owner>/proxy-systems/tor-proxy:<version>
```
