# Hydra

[English](README.md)

**高性能 LLM 路由网关。** 将 OpenAI 兼容的客户端流量路由到上游模型供应商，提供按租户鉴权、加权负载均衡、故障转移、熔断、限流、用量计量、按租户 TLS——全程零拷贝热路径。基于 Rust + [Pingora](https://github.com/cloudflare/pingora)。

---

## 这是什么

Hydra 部署在你的 Agent/客户端与 LLM 供应商之间。一次请求：按域名解析租户 → 调用租户自有认证端点鉴权 → 路由（模型 × 租户授权供应商，加权轮询）→ 把客户端 key 换成供应商 key → 原样转发 → 从 SSE 响应解析用量 → 记录。供应商失败时自动切换到下一个候选。

```
Agent ──► Hydra ──► [解析租户 → 外部认证 → 路由 → 换 key → 转发]
                          │                    │
                   租户认证服务            LLM / 多模态供应商
                          │                    │
                          └─► 缓存 5 分钟          └─► SSE 流式回写，计量用量
```

## 特性

- **路由**：模型名 → 供应商 ∩ 租户授权供应商；平滑加权轮询（Nginx SWRR）。
- **外部认证**：每个租户配置自己的 `auth_url`；Hydra 缓存判定 5 分钟，并提供失效接口（欠费/封禁由租户自决）。
- **故障转移 + 熔断**：连接失败自动切下一供应商；连续失败触发 dead-set，后台探活恢复。
- **限流**：内存滑动窗口（请求数 + token），按角色，m/h/d 窗口。
- **用量记录**：可插拔 Sink（默认 SQLite，可选 ClickHouse）；从 SSE 流解析 token 用量。
- **按租户 TLS**：基于 SNI 的证书选择，热更新。
- **零拷贝**：请求/响应 body 原样透传；`model`/`usage` 用 `memchr` 扫描提取（不做整体 JSON 往返）。
- **管理 REST + UI**：全部配置实体增删改查、Prometheus `/metrics`、内嵌控制台。

## 部署

### Docker（推荐）

```bash
# 1. 交叉编译 linux/amd64 二进制 + 构建镜像
./environment/build.sh

# 2. 运行
docker run -d --name hydra \
  -p 443:443 -p 8080:8080 -p 8081:8081 \
  -e HYDRA_ADMIN_TOKEN=<你的管理 token> \
  -e HYDRA_ADMIN_ADDR=0.0.0.0:8081 \
  -v "$PWD/data":/app/data \
  hydra:latest
```

> `build.sh` 流程：`rust_build_linux`（在 `crates/hydra-server/` 下）→ 暂存 `bin/hydra` → `docker build`。镜像固定 `linux/amd64` 以匹配交叉编译二进制（Apple Silicon 上走 Rosetta/qemu）。

### 源码编译

```bash
cargo build --release --features server
# 二进制：target/release/hydra（或 ~/.cargo/global-target/release/hydra）
HYDRA_ADMIN_TOKEN=<token> ./target/release/hydra
```

## 配置

Hydra 通过**环境变量**启动（运行时），所有路由配置存于 **SQLite**（经管理 API 管理）。

| 环境变量             | 默认值                           | 用途                                                |
| -------------------- | -------------------------------- | --------------------------------------------------- |
| `HYDRA_DB_URL`       | `sqlite:hydra.db?mode=rwc`       | SQLite 数据库位置                                   |
| `HYDRA_LISTEN`       | `0.0.0.0:8080`                   | 代理监听地址（配证书时用 `:443` 走 TLS）            |
| `HYDRA_ADMIN_ADDR`   | `127.0.0.1:8081`                 | 管理 REST + UI + `/metrics` 监听地址                |
| `HYDRA_ADMIN_TOKEN`  | —                                | 守护 `/api/v1/*` 的 Bearer token（**管理必填**）     |
| `HYDRA_USAGE_SINK`   | `sqlite`                         | `sqlite` 或 `clickhouse`                            |
| `RUST_LOG`           | `info`                           | 日志级别                                            |

**端口**：`8080`/`443` 代理 · `8081` 管理（REST + UI + metrics）。

**配置数据模型**（在 SQLite，经 `/api/v1/*`）：`provider`、`provider-model`、`provider-key`、`tenant`（含 `auth_url`）、`tenant-provider`、`tenant-model`、`limit-role`。完整 schema 见 `docs/design.md` §4。

## 使用

### 管理界面

打开 `http://<host>:8081/admin/`，输入管理 token。管理供应商、模型、key、租户、授权、限流角色，查看/失效认证缓存与熔断器。

### 管理 REST

```bash
TOKEN=<你的管理 token>

# 新建供应商
curl -X POST http://localhost:8081/api/v1/providers \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"id":"openai","key":"openai","name":"OpenAI","endpoint":"https://api.openai.com","weight":1}'

# 新建租户（auth_url 必填）+ 授权供应商与模型
curl -X POST http://localhost:8081/api/v1/tenants \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"id":"acme","name":"ACME","domain":"acme.example.com","auth_url":"https://auth.acme.example.com/v","enabled":true}'

# 列表 / 重载 / 指标
curl -H "Authorization: Bearer $TOKEN" http://localhost:8081/api/v1/providers
curl -X POST -H "Authorization: Bearer $TOKEN" http://localhost:8081/api/v1/reload
curl http://localhost:8081/metrics
```

### 把客户端指向 Hydra

任意 OpenAI 兼容客户端：把 base URL 指向代理，带上租户的客户端 api-key。

```bash
curl https://acme.example.com/v1/chat/completions \   # 或 http://<hydra>:8080/v1
  -H "Authorization: Bearer <客户端 api-key>" \
  -H "Content-Type: application/json" \
  -d '{"model":"gpt-4o","messages":[{"role":"user","content":"你好"}],"stream":true}'
```

Hydra 按域名解析租户 `acme` → 调 `auth_url` 鉴权 key → 把 `gpt-4o` 路由到授权供应商 → 换上供应商 key → 流式回写响应 → 记录用量。

## 工程结构

```
crates/hydra-core/    纯领域逻辑（路由、SWRR、熔断、SSE 扫描、限流）——零 I/O 依赖
crates/hydra-server/  Pingora 代理外壳、DB、认证、用量 Sink、TLS、管理 API
docs/                 design.md、dev-plan.md、ops.md、波次计划
environment/          Dockerfile + build.sh（linux/amd64 运行时）
integration/          Python CRUD 测试套件 + docker 运行器
```

## 更多

- 设计与架构：[`docs/design.md`](docs/design.md)
- 运维手册：[`docs/ops.md`](docs/ops.md)
- 开发计划（TDD、禁 Mock、零拷贝）：[`docs/dev-plan.md`](docs/dev-plan.md)

Rust 1.83+ · Pingora 0.8.x · License：见仓库。
