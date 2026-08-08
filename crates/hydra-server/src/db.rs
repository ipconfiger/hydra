//! sqlx `SqlitePool`, PRAGMA setup (design §15.2), `sqlx::migrate!` embedding,
//! and the repo CRUD layer (thin wrappers over runtime-checked sqlx queries).
//!
//! ## Compile-time vs runtime SQL
//!
//! `query!`/`query_as!` require either a `DATABASE_URL` pointing at an
//! already-migrated database or a committed `.sqlx` offline cache produced by
//! `cargo sqlx prepare`. The test harness in this crate uses a fresh
//! `sqlite::memory:` database per test (design §3 of `wave-2-persistence.md`),
//! which cannot be introspected at compile time — each in-memory connection is
//! an isolated, empty database. We therefore use **runtime-checked**
//! `query`/`query_as` here (one of the explicit options permitted by the wave
//! spec). Switching to `query!` later is a mechanical follow-up once a
//! `sqlx prepare` offline cache exists; it changes no behaviour, only the
//! compile-time guarantee.
//!
//! Repo functions map the SQLite `INTEGER` columns onto `i64` in the row
//! structs (sqlx decodes `INTEGER` → `i64`) and cast to the idiomatic `i32` /
//! `bool` used by [`hydra_core::model`] at the boundary.

use std::str::FromStr;
use std::time::Duration;

use sqlx::migrate::MigrateError;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::SqlitePool;

use hydra_core::model::{
    LimitRole, Provider, ProviderKey, ProviderModel, Tenant, TenantModel, TenantProvider,
};

// ---------------------------------------------------------------------------
// Pool initialisation + PRAGMAs (design §15.2)
// ---------------------------------------------------------------------------

/// Connect a `SqlitePool` and apply the production PRAGMAs (design §15.2):
/// `journal_mode=WAL`, `synchronous=NORMAL`, `busy_timeout=5000`,
/// `foreign_keys=ON`, `mmap_size=134217728`.
///
/// PRAGMAs are applied **per connection** via [`SqliteConnectOptions`]:
/// `foreign_keys` is connection-scoped in SQLite, so a one-shot
/// `PRAGMA foreign_keys=ON` on a single connection would not propagate to the
/// rest of the pool — the connect-options builder applies it to every fresh
/// connection, which is the root-cause-correct approach for a multi-connection
/// file-backed production pool.
///
/// For `:memory:` URLs the pool is pinned to a single connection: each
/// in-memory connection is otherwise an isolated empty database, so a pool
/// with more than one connection would make migrations invisible to other
/// connections (the canonical sqlx in-memory test pattern).
pub async fn init_pool(url: &str) -> Result<SqlitePool, sqlx::Error> {
    let opts = SqliteConnectOptions::from_str(url)?
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .busy_timeout(Duration::from_millis(5000))
        .foreign_keys(true)
        .pragma("mmap_size", "134217728");

    let is_memory = url.contains(":memory:");
    let pool = SqlitePoolOptions::new()
        .max_connections(if is_memory { 1 } else { 8 })
        .connect_with(opts)
        .await?;
    Ok(pool)
}

/// Run the embedded migrations (`sqlx::migrate!("./migrations")`).
pub async fn run_migrate(pool: &SqlitePool) -> Result<(), MigrateError> {
    sqlx::migrate!("./migrations").run(pool).await
}

// ---------------------------------------------------------------------------
// Row structs (mirror SQLite types) + conversions to core entities.
// ---------------------------------------------------------------------------

#[derive(sqlx::FromRow, Debug, Clone)]
struct ProviderRow {
    id: String,
    key: String,
    name: String,
    endpoint: String,
    weight: i64,
    created_at: String,
    updated_at: String,
}

impl From<ProviderRow> for Provider {
    fn from(r: ProviderRow) -> Self {
        Provider {
            id: r.id,
            key: r.key,
            name: r.name,
            endpoint: r.endpoint,
            weight: r.weight as i32,
            created_at: r.created_at,
            updated_at: r.updated_at,
        }
    }
}

#[derive(sqlx::FromRow, Debug, Clone)]
struct ProviderModelRow {
    id: String,
    key: String,
    name: String,
    provider_id: String,
    status: i64,
}

impl From<ProviderModelRow> for ProviderModel {
    fn from(r: ProviderModelRow) -> Self {
        ProviderModel {
            id: r.id,
            key: r.key,
            name: r.name,
            provider_id: r.provider_id,
            status: r.status as i32,
        }
    }
}

#[derive(sqlx::FromRow, Debug, Clone)]
struct ProviderKeyRow {
    id: String,
    provider_id: String,
    api_key: String,
    created_at: String,
}

impl From<ProviderKeyRow> for ProviderKey {
    fn from(r: ProviderKeyRow) -> Self {
        ProviderKey {
            id: r.id,
            provider_id: r.provider_id,
            api_key: r.api_key,
            created_at: r.created_at,
        }
    }
}

#[derive(sqlx::FromRow, Debug, Clone)]
struct TenantRow {
    id: String,
    name: String,
    domain: String,
    auth_url: String,
    cert_key: Option<String>,
    cert_file: Option<String>,
    enabled: i64,
    created_at: String,
    updated_at: String,
}

impl From<TenantRow> for Tenant {
    fn from(r: TenantRow) -> Self {
        Tenant {
            id: r.id,
            name: r.name,
            domain: r.domain,
            auth_url: r.auth_url,
            cert_key: r.cert_key,
            cert_file: r.cert_file,
            enabled: r.enabled != 0,
            created_at: r.created_at,
            updated_at: r.updated_at,
        }
    }
}

#[derive(sqlx::FromRow, Debug, Clone)]
struct TenantProviderRow {
    id: String,
    tenant_id: String,
    provider_id: String,
}

impl From<TenantProviderRow> for TenantProvider {
    fn from(r: TenantProviderRow) -> Self {
        TenantProvider {
            id: r.id,
            tenant_id: r.tenant_id,
            provider_id: r.provider_id,
        }
    }
}

#[derive(sqlx::FromRow, Debug, Clone)]
struct TenantModelRow {
    id: String,
    tenant_id: String,
    model_key: String,
}

impl From<TenantModelRow> for TenantModel {
    fn from(r: TenantModelRow) -> Self {
        TenantModel {
            id: r.id,
            tenant_id: r.tenant_id,
            model_key: r.model_key,
        }
    }
}

#[derive(sqlx::FromRow, Debug, Clone)]
struct LimitRoleRow {
    id: String,
    name: String,
    matching_key: Option<String>,
    matching_model: Option<String>,
    matching_tenant: Option<String>,
    matching_provider: Option<String>,
    limit_count: Option<i64>,
    limit_token: Option<i64>,
    window: String,
    enabled: i64,
    created_at: String,
}

impl From<LimitRoleRow> for LimitRole {
    fn from(r: LimitRoleRow) -> Self {
        LimitRole {
            id: r.id,
            name: r.name,
            matching_key: r.matching_key,
            matching_model: r.matching_model,
            matching_tenant: r.matching_tenant,
            matching_provider: r.matching_provider,
            limit_count: r.limit_count,
            limit_token: r.limit_token,
            window: r.window,
            enabled: r.enabled != 0,
            created_at: r.created_at,
        }
    }
}

// ---------------------------------------------------------------------------
// CRUD — Provider
// ---------------------------------------------------------------------------

/// Insert a provider. Violating the UNIQUE `key` constraint returns
/// [`sqlx::Error::Database`] (UNIQUE violation).
pub async fn insert_provider(pool: &SqlitePool, p: &Provider) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO provider (id, key, name, endpoint, weight, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&p.id)
    .bind(&p.key)
    .bind(&p.name)
    .bind(&p.endpoint)
    .bind(p.weight)
    .bind(&p.created_at)
    .bind(&p.updated_at)
    .execute(pool)
    .await?;
    Ok(())
}

/// Fetch a single provider by id; `Err(RowNotFound)` when absent.
pub async fn get_provider(pool: &SqlitePool, id: &str) -> Result<Provider, sqlx::Error> {
    let row: ProviderRow = sqlx::query_as("SELECT * FROM provider WHERE id = ?")
        .bind(id)
        .fetch_one(pool)
        .await?;
    Ok(row.into())
}

/// All providers, ordered by `created_at`.
pub async fn list_providers(pool: &SqlitePool) -> Result<Vec<Provider>, sqlx::Error> {
    let rows: Vec<ProviderRow> = sqlx::query_as("SELECT * FROM provider ORDER BY created_at")
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

/// Update a provider's mutable fields.
pub async fn update_provider(pool: &SqlitePool, p: &Provider) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE provider SET key = ?, name = ?, endpoint = ?, weight = ?, updated_at = ? \
         WHERE id = ?",
    )
    .bind(&p.key)
    .bind(&p.name)
    .bind(&p.endpoint)
    .bind(p.weight)
    .bind(&p.updated_at)
    .bind(&p.id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Delete a provider (CASCADE removes its models/keys and tenant_provider links).
pub async fn delete_provider(pool: &SqlitePool, id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM provider WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// CRUD — ProviderModel
// ---------------------------------------------------------------------------

/// Insert a provider model. The CHECK (`status IN (1,0,-1)`) and
/// UNIQUE(`key`,`provider_id`) constraints are enforced by SQLite.
pub async fn insert_provider_model(
    pool: &SqlitePool,
    m: &ProviderModel,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO provider_model (id, key, name, provider_id, status) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(&m.id)
    .bind(&m.key)
    .bind(&m.name)
    .bind(&m.provider_id)
    .bind(m.status)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get_provider_model(pool: &SqlitePool, id: &str) -> Result<ProviderModel, sqlx::Error> {
    let row: ProviderModelRow = sqlx::query_as("SELECT * FROM provider_model WHERE id = ?")
        .bind(id)
        .fetch_one(pool)
        .await?;
    Ok(row.into())
}

pub async fn list_provider_models(pool: &SqlitePool) -> Result<Vec<ProviderModel>, sqlx::Error> {
    let rows: Vec<ProviderModelRow> =
        sqlx::query_as("SELECT * FROM provider_model ORDER BY key, provider_id")
            .fetch_all(pool)
            .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

/// Update a provider model's mutable fields (`key`, `name`, `status`).
pub async fn update_provider_model(
    pool: &SqlitePool,
    m: &ProviderModel,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE provider_model SET key = ?, name = ?, status = ? WHERE id = ?")
        .bind(&m.key)
        .bind(&m.name)
        .bind(m.status)
        .bind(&m.id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn delete_provider_model(pool: &SqlitePool, id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM provider_model WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// CRUD — ProviderKey
// ---------------------------------------------------------------------------

pub async fn insert_provider_key(pool: &SqlitePool, k: &ProviderKey) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO provider_key (id, provider_id, api_key, created_at) VALUES (?, ?, ?, ?)",
    )
    .bind(&k.id)
    .bind(&k.provider_id)
    .bind(&k.api_key)
    .bind(&k.created_at)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get_provider_key(pool: &SqlitePool, id: &str) -> Result<ProviderKey, sqlx::Error> {
    let row: ProviderKeyRow = sqlx::query_as("SELECT * FROM provider_key WHERE id = ?")
        .bind(id)
        .fetch_one(pool)
        .await?;
    Ok(row.into())
}

/// All provider keys (the loader groups them by `provider_id`).
pub async fn list_provider_keys(pool: &SqlitePool) -> Result<Vec<ProviderKey>, sqlx::Error> {
    let rows: Vec<ProviderKeyRow> =
        sqlx::query_as("SELECT * FROM provider_key ORDER BY provider_id, created_at")
            .fetch_all(pool)
            .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

/// Keys belonging to a single provider (ordered by creation).
pub async fn list_provider_keys_by_provider(
    pool: &SqlitePool,
    provider_id: &str,
) -> Result<Vec<ProviderKey>, sqlx::Error> {
    let rows: Vec<ProviderKeyRow> =
        sqlx::query_as("SELECT * FROM provider_key WHERE provider_id = ? ORDER BY created_at")
            .bind(provider_id)
            .fetch_all(pool)
            .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

pub async fn delete_provider_key(pool: &SqlitePool, id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM provider_key WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// CRUD — Tenant
// ---------------------------------------------------------------------------

/// Insert a tenant. `auth_url` is NOT NULL → inserting with an empty/non-null
/// value is fine; a NULL would error at the SQLite layer.
pub async fn insert_tenant(pool: &SqlitePool, t: &Tenant) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO tenant (id, name, domain, auth_url, cert_key, cert_file, enabled, \
         created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&t.id)
    .bind(&t.name)
    .bind(&t.domain)
    .bind(&t.auth_url)
    .bind(&t.cert_key)
    .bind(&t.cert_file)
    .bind(t.enabled)
    .bind(&t.created_at)
    .bind(&t.updated_at)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get_tenant(pool: &SqlitePool, id: &str) -> Result<Tenant, sqlx::Error> {
    let row: TenantRow = sqlx::query_as("SELECT * FROM tenant WHERE id = ?")
        .bind(id)
        .fetch_one(pool)
        .await?;
    Ok(row.into())
}

pub async fn list_tenants(pool: &SqlitePool) -> Result<Vec<Tenant>, sqlx::Error> {
    let rows: Vec<TenantRow> = sqlx::query_as("SELECT * FROM tenant ORDER BY created_at")
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

/// Update a tenant's mutable fields (`name`, `domain`, `auth_url`, cert paths,
/// `enabled`, `updated_at`).
pub async fn update_tenant(pool: &SqlitePool, t: &Tenant) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE tenant SET name = ?, domain = ?, auth_url = ?, cert_key = ?, cert_file = ?, \
         enabled = ?, updated_at = ? WHERE id = ?",
    )
    .bind(&t.name)
    .bind(&t.domain)
    .bind(&t.auth_url)
    .bind(&t.cert_key)
    .bind(&t.cert_file)
    .bind(t.enabled)
    .bind(&t.updated_at)
    .bind(&t.id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn delete_tenant(pool: &SqlitePool, id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM tenant WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// CRUD — TenantProvider
// ---------------------------------------------------------------------------

pub async fn insert_tenant_provider(
    pool: &SqlitePool,
    tp: &TenantProvider,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO tenant_provider (id, tenant_id, provider_id) VALUES (?, ?, ?)")
        .bind(&tp.id)
        .bind(&tp.tenant_id)
        .bind(&tp.provider_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn get_tenant_provider(
    pool: &SqlitePool,
    id: &str,
) -> Result<TenantProvider, sqlx::Error> {
    let row: TenantProviderRow = sqlx::query_as("SELECT * FROM tenant_provider WHERE id = ?")
        .bind(id)
        .fetch_one(pool)
        .await?;
    Ok(row.into())
}

pub async fn list_tenant_providers(pool: &SqlitePool) -> Result<Vec<TenantProvider>, sqlx::Error> {
    let rows: Vec<TenantProviderRow> =
        sqlx::query_as("SELECT * FROM tenant_provider ORDER BY tenant_id, provider_id")
            .fetch_all(pool)
            .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

pub async fn delete_tenant_provider(pool: &SqlitePool, id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM tenant_provider WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// CRUD — TenantModel
// ---------------------------------------------------------------------------

pub async fn insert_tenant_model(pool: &SqlitePool, tm: &TenantModel) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO tenant_model (id, tenant_id, model_key) VALUES (?, ?, ?)")
        .bind(&tm.id)
        .bind(&tm.tenant_id)
        .bind(&tm.model_key)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn get_tenant_model(pool: &SqlitePool, id: &str) -> Result<TenantModel, sqlx::Error> {
    let row: TenantModelRow = sqlx::query_as("SELECT * FROM tenant_model WHERE id = ?")
        .bind(id)
        .fetch_one(pool)
        .await?;
    Ok(row.into())
}

pub async fn list_tenant_models(pool: &SqlitePool) -> Result<Vec<TenantModel>, sqlx::Error> {
    let rows: Vec<TenantModelRow> =
        sqlx::query_as("SELECT * FROM tenant_model ORDER BY tenant_id, model_key")
            .fetch_all(pool)
            .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

pub async fn delete_tenant_model(pool: &SqlitePool, id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM tenant_model WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// CRUD — LimitRole
// ---------------------------------------------------------------------------

pub async fn insert_limit_role(pool: &SqlitePool, r: &LimitRole) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO limit_role (id, name, matching_key, matching_model, matching_tenant, \
         matching_provider, limit_count, limit_token, window, enabled, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&r.id)
    .bind(&r.name)
    .bind(&r.matching_key)
    .bind(&r.matching_model)
    .bind(&r.matching_tenant)
    .bind(&r.matching_provider)
    .bind(r.limit_count)
    .bind(r.limit_token)
    .bind(&r.window)
    .bind(r.enabled)
    .bind(&r.created_at)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get_limit_role(pool: &SqlitePool, id: &str) -> Result<LimitRole, sqlx::Error> {
    let row: LimitRoleRow = sqlx::query_as("SELECT * FROM limit_role WHERE id = ?")
        .bind(id)
        .fetch_one(pool)
        .await?;
    Ok(row.into())
}

pub async fn list_limit_roles(pool: &SqlitePool) -> Result<Vec<LimitRole>, sqlx::Error> {
    let rows: Vec<LimitRoleRow> = sqlx::query_as("SELECT * FROM limit_role ORDER BY created_at")
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

/// Update a limit role's mutable fields.
pub async fn update_limit_role(pool: &SqlitePool, r: &LimitRole) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE limit_role SET name = ?, matching_key = ?, matching_model = ?, \
         matching_tenant = ?, matching_provider = ?, limit_count = ?, limit_token = ?, \
         window = ?, enabled = ? WHERE id = ?",
    )
    .bind(&r.name)
    .bind(&r.matching_key)
    .bind(&r.matching_model)
    .bind(&r.matching_tenant)
    .bind(&r.matching_provider)
    .bind(r.limit_count)
    .bind(r.limit_token)
    .bind(&r.window)
    .bind(r.enabled)
    .bind(&r.id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn delete_limit_role(pool: &SqlitePool, id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM limit_role WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}
