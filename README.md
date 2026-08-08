# Hydra

**A high-performance LLM routing gateway.** Route OpenAI-compatible client traffic to upstream model providers with per-tenant auth, weighted load balancing, failover, circuit breaking, rate limiting, usage metering, and per-tenant TLS — all on a zero-copy hot path. Built in Rust on [Pingora](https://github.com/cloudflare/pingora).

[中文文档](README.zh-CN.md)

---

## What it is

Hydra sits between your agents/clients and your LLM providers. A client request is resolved to a tenant by domain, authenticated against the tenant's own auth endpoint, routed (model × tenant-allowed providers, weighted round-robin), the client key is swapped for a provider key, the request is streamed through untouched, usage is parsed from the SSE response, and the result is recorded. If a provider fails, Hydra fails over to the next candidate automatically.

```
Agent ──► Hydra ──► [tenant resolve → external auth → route → swap key → forward]
                                  │                        │
                       tenant auth service          LLM / media provider
                                  │                        │
                                  └─► cached 5 min          └─► SSE streamed back, usage metered
```

## Features

- **Routing**: model name → providers ∩ tenant-allowed providers; smooth weighted round-robin (Nginx SWRR).
- **External auth**: each tenant points to its own `auth_url`; Hydra caches verdicts 5 min and exposes an invalidation endpoint (the tenant decides欠费/封禁).
- **Failover + circuit breaker**: connection failures auto-retry next provider; consecutive failures trip a dead-set with background probing.
- **Rate limiting**: in-memory sliding window (request count + token), per role, m/h/d windows.
- **Usage recording**: pluggable sink (SQLite default, ClickHouse optional); token counts parsed from the SSE stream.
- **Per-tenant TLS**: SNI-based certificate selection with hot-reload.
- **Zero-copy**: request/response bodies flow through untouched; `model` and `usage` are extracted by `memchr` scan (no full-body JSON round-trip).
- **Admin REST + UI**: full CRUD for all config entities, Prometheus `/metrics`, embedded dashboard.

## Deploy

### Docker (recommended)

```bash
# 1. cross-compile the linux/amd64 binary + build the image
./environment/build.sh

# 2. run
docker run -d --name hydra \
  -p 443:443 -p 8080:8080 -p 8081:8081 \
  -e HYDRA_ADMIN_TOKEN=<your-admin-token> \
  -e HYDRA_ADMIN_ADDR=0.0.0.0:8081 \
  -v "$PWD/data":/app/data \
  hydra:latest
```

> `build.sh` runs `rust_build_linux` (from `crates/hydra-server/`) → stages `bin/hydra` → `docker build`. The image is pinned `linux/amd64` to match the cross-compiled binary (runs under Rosetta/qemu on Apple Silicon).

### From source

```bash
cargo build --release --features server
# binary: target/release/hydra   (or ~/.cargo/global-target/release/hydra)
HYDRA_ADMIN_TOKEN=<token> ./target/release/hydra
```

## Configure

Hydra boots from **environment variables** (runtime) and stores all routing config in **SQLite** (managed via the admin API).

| Env var              | Default                          | Purpose                                              |
| -------------------- | -------------------------------- | ---------------------------------------------------- |
| `HYDRA_DB_URL`       | `sqlite:hydra.db?mode=rwc`       | SQLite database location                             |
| `HYDRA_LISTEN`       | `0.0.0.0:8080`                   | Proxy listen address (use `:443` + certs for TLS)    |
| `HYDRA_ADMIN_ADDR`   | `127.0.0.1:8081`                 | Admin REST + UI + `/metrics` listen address          |
| `HYDRA_ADMIN_TOKEN`  | —                                | Bearer token gating `/api/v1/*` (**required for admin**) |
| `HYDRA_USAGE_SINK`   | `sqlite`                         | `sqlite` or `clickhouse`                             |
| `RUST_LOG`           | `info`                           | Log level                                            |

**Ports**: `8080`/`443` proxy · `8081` admin (REST + UI + metrics).

**Config data model** (in SQLite, via `/api/v1/*`): `provider`, `provider-model`, `provider-key`, `tenant` (with `auth_url`), `tenant-provider`, `tenant-model`, `limit-role`. Full schema in `docs/design.md` §4.

## Use

### Admin UI

Open `http://<host>:8081/admin/` and enter the admin token. Manage providers, models, keys, tenants, access, rate-limit roles, and view/invalidate the auth cache and circuit breaker.

### Admin REST

```bash
TOKEN=<your-admin-token>

# create a provider
curl -X POST http://localhost:8081/api/v1/providers \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"id":"openai","key":"openai","name":"OpenAI","endpoint":"https://api.openai.com","weight":1}'

# create a tenant (auth_url mandatory) + grant provider + model access
curl -X POST http://localhost:8081/api/v1/tenants \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"id":"acme","name":"ACME","domain":"acme.example.com","auth_url":"https://auth.acme.example.com/v","enabled":true}'

# list / reload / metrics
curl -H "Authorization: Bearer $TOKEN" http://localhost:8081/api/v1/providers
curl -X POST -H "Authorization: Bearer $TOKEN" http://localhost:8081/api/v1/reload
curl http://localhost:8081/metrics
```

### Point a client at Hydra

Any OpenAI-compatible client: set the base URL to the proxy and send the tenant's client api-key.

```bash
curl https://acme.example.com/v1/chat/completions \   # or http://<hydra>:8080/v1
  -H "Authorization: Bearer <client-api-key>" \
  -H "Content-Type: application/json" \
  -d '{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}],"stream":true}'
```

Hydra resolves tenant `acme` by domain → calls `auth_url` to authorize the key → routes `gpt-4o` to an allowed provider → swaps in a provider key → streams the response back → records usage.

## Project layout

```
crates/hydra-core/    pure domain logic (router, SWRR, breaker, SSE scan, limits) — zero I/O deps
crates/hydra-server/  Pingora proxy shell, DB, auth, usage sink, TLS, admin
docs/                 design.md, dev-plan.md, ops.md, wave plans
environment/          Dockerfile + build.sh (linux/amd64 runtime)
integration/          Python CRUD test suite + docker runner
```

## More

- Design & architecture: [`docs/design.md`](docs/design.md)
- Operations runbook: [`docs/ops.md`](docs/ops.md)
- Development plan (TDD, no-mock, zero-copy): [`docs/dev-plan.md`](docs/dev-plan.md)

Rust 1.83+ · Pingora 0.8.x · License: see repository.
