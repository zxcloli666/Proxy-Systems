# Proxy Systems

Chain of proxy servers written in Rust for routing and load-balancing HTTP requests.

```
Client -> [Intermediate Proxy :3000] -> [Simple Proxy / Simple IPv6 Proxy / Tor Proxy / Cloudflare Workers / HTTP proxy / SOCKS5 proxy]
```

## Components

### simple-proxy

Basic reverse proxy. Receives the target URL as a base64-encoded `X-Target` header, forwards the request, and streams the response back with CORS headers. Handles redirects (relative, absolute, protocol-relative).

**Port:** `8080` (env `PORT`)

| Env | Description | Default |
|-----|-------------|---------|
| `AUTH_TOKEN` | When set, callers must send `X-Proxy-Auth: <token>` or get `401`. Compared in constant time and stripped before forwarding, so the token never reaches the target. Empty/unset = open proxy (default). | — |
| `IMPERSONATE` | Browser profile for the outgoing TLS/HTTP2 fingerprint (`chrome_133`, `firefox_136`, `safari_18`, …). Unset = plain rustls over HTTP/1.1 (default, unchanged behavior). | — |

**Why `IMPERSONATE` matters**: the proxy — not the caller — terminates TLS with the target, so the target sees *its* fingerprint. Defaults look like `t13d1011h1_…` (rustls, HTTP/1.1), which several Russian classifieds now block outright: measured on the same IP in the same second, plain curl got `200` with real data while this proxy got a captcha redirect. With `IMPERSONATE=chrome_133` the JA4 becomes `t13d1516h2_…` over h2 with Chrome's Akamai HTTP/2 signature. Profile names are the serde names of `wreq_util::Emulation`.

Set it on any instance reachable from the internet — an open `X-Target` proxy will be found and abused. `intermediate-proxy` can attach the header per route with `add_headers` in `routes.lua`.

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

Lua-programmable router with a **per-route × per-proxy** health matrix. A proxy
banned on `api-v2.soundcloud.com` is still used for `genius.com`; bans, probes,
timeouts and the selector are all decided per route by an embedded Lua config.

- **Per-route health** — every `(route, proxy)` pair tracks its own tier
  (`healthy`/`slow`/`failed`/`banned`/`fatal`), EWMA latency and ban expiry.
  Only a connect-class failure (connection refused / DNS / connect error)
  marks the proxy fatal *globally* (unreachable everywhere) — and a fast
  global prober (`TRANSPORT_PROBE_*`, ~10 s) restores it the moment it
  answers again. A request *timeout* is only a per-route soft failure (a slow
  target must not sideline a healthy proxy for every route); everything else
  is a per-route ban.
- **Lua classification** — `on_response(ctx, res)` returns a verdict:
  `success`, `return_as_is` (target's fault — proxy fine), `retry_other_proxy`
  (ban this proxy on this route for `ban_for_ms`, try the next),
  `retry_same_proxy`, `hard_fail`, or `give_up`. Lua errors fail open to
  `success`. CPU-only, run on a pool of sandboxed VMs with an instruction
  budget.
- **Route matching** — first route whose `host`/`host_regex` + `path`/`path_regex`
  + `methods` match, then an optional Lua `predicate(req)`. No match → `502`.
- **Selectors (per route)** — `best_latency`, `round_robin`, `random`,
  `sticky_by_header`, or `hedge` (parallel race; `max_parallel`/`delay_ms` set
  in Lua, never auto).
- **Per-route probes** — each route declares its own `probe { url,
  interval_ms, ok_statuses, classify }`. Banned proxies are re-probed on that
  schedule (bounded by `MAX_CONCURRENT_PROBES`) and unbanned on success.
- **`on_exhausted(ctx)`** — when every live proxy failed: `try_fatal`
  (last-resort over banned/fatal), `wait_probe`, `retry_all`, or
  `return_error`. A global `MAX_HARD_ATTEMPTS` cap stops Lua retry loops.
- **Hot reload, no restart** — `proxies.txt` and `routes.lua` are polled by
  mtime. The proxy list diffs by URL so live proxies keep their stats; a
  broken `routes.lua` is logged and the previous config keeps serving (never
  half-applied).
- **Proxy tags** — `socks5://h:1080 tags=anon,us`; a route picks a pool with
  `pool = { tags = {"anon"} }`. `prefer_tags = {"ipv6"}` floats tagged proxies
  to the front of the attempt plan (health-aware — a banned preferred proxy is
  skipped, not forced); `exclude_tags = {"ipv6"}` makes tagged proxies
  ineligible for the route entirely (also skipped in the fatal fallback). Both
  are settable in `defaults` and overridable per route with `{}`.
  The `reserve` tag means "only when every non-reserve proxy is down".
- **Stream recovery** — mid-stream failures on cacheable/media GETs resume
  from the next proxy via `Range` (proxy-managed, not Lua-configurable).
- **Upstream types** by URL scheme: `http(s)://` endpoint (`X-Target` header),
  `socks5://` SOCKS5, `forward://` HTTP forward proxy. HTTP/1 only.

Wire API is unchanged: clients still send the base64 target in `X-Target`.

**Port:** `3000` (env `PORT`)

| Env | Description | Default |
|-----|-------------|---------|
| `PROXY_FILE` | Path to the proxy list (hot-reloaded) | `/etc/proxies.txt` |
| `PROXY_REFRESH_MS` | Proxy file mtime poll interval | `30000` |
| `ROUTES_FILE` | Path to the Lua routing config (hot-reloaded) | `/etc/routes.lua` |
| `ROUTES_REFRESH_MS` | Routes file mtime poll interval | `30000` |
| `MAX_CONCURRENT_PROBES` | Global cap on in-flight probe requests | `16` |
| `LUA_VM_COUNT` | Number of pooled Lua VMs | `cpus×2` (min 4) |
| `LUA_INSTRUCTION_LIMIT` | Per-call Lua instruction budget (0 = off) | `1000000` |
| `MAX_HARD_ATTEMPTS` | Hard cap on `retry_all`/`wait_probe` rounds | `20` |
| `TRANSPORT_PROBE_URL` | URL the fast global prober uses to recover connect-dead proxies (empty disables) | `https://www.google.com/generate_204` |
| `TRANSPORT_PROBE_INTERVAL_MS` | How often connect-fatal proxies are re-probed | `10000` |
| `TRANSPORT_PROBE_TIMEOUT_MS` | Per-probe timeout for the transport prober | `8000` |

`routes.lua` missing or unparseable at startup is fatal (the proxy can't route
without it). `proxies.txt` missing starts empty and warns — the watcher picks
it up when it appears. See `intermediate-proxy/examples/` for a commented
`proxies.txt` and a production-shaped `routes.lua` (SoundCloud v1/v2, Genius
token vs scrape, lyrics sites, catch-all).

**Health endpoints:**
- `GET /health` — proxy totals + per-route tier counts + probe config.
- `GET /health/route/{name}` — every proxy's state for one route.
- `GET /health/proxy?url=<proxy-url>` — one proxy across all routes it has seen.

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

Один общий `Dockerfile` с multi-stage targets — workspace компилится один раз
для любой комбинации из четырёх бинарей:

```bash
DOCKER_BUILDKIT=1 docker build --target simple-proxy        -t simple-proxy        .
DOCKER_BUILDKIT=1 docker build --target simple-ipv6-proxy   -t simple-ipv6-proxy   .
DOCKER_BUILDKIT=1 docker build --target intermediate-proxy  -t intermediate-proxy  .
DOCKER_BUILDKIT=1 docker build --target tor-proxy           -t tor-proxy           .
```

> BuildKit нужен из-за `--mount=type=cache` для cargo registry. На современных
> Docker Engine BuildKit включён по умолчанию.

## Release

Push to `main` with `!release: patch`, `!release: minor`, or `!release: major` in the commit message. GitHub Action builds and pushes images to GHCR:

```
ghcr.io/zxcloli666/proxy-systems/simple-proxy:latest
ghcr.io/zxcloli666/proxy-systems/simple-ipv6-proxy:latest
ghcr.io/zxcloli666/proxy-systems/intermediate-proxy:latest
ghcr.io/zxcloli666/proxy-systems/tor-proxy:latest
```
