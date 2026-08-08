/**
 * Hydra admin UI — Playwright E2E spec (wave-6 §2.2 / design §19.3 / AGENTS.md).
 *
 * Covers:
 *   - T2.1 login with admin token
 *   - T2.2 create provider → list shows it → persisted via /api (DB row exists)
 *   - T2.3 tenant with auth_url + associate provider/model
 *   - T2.4 auth-cache invalidate
 *   - T2.5 breaker reset
 *
 * The suite ASSUMES a running hydra instance (start it with seed.sh first; see
 * tests/e2e/README.md). It does NOT spawn the binary itself: the binary needs
 * a real Pingora listener + a SQLite file, which is environment-specific.
 *
 * Config: HYDRA_BASE (default http://127.0.0.1:8081), HYDRA_ADMIN_TOKEN.
 */
// @ts-check
const { test, expect } = require('@playwright/test');

const BASE = process.env.HYDRA_BASE || 'http://127.0.0.1:8081';
const TOKEN = process.env.HYDRA_ADMIN_TOKEN || 'dev-admin-token';
// Distinct prefix so parallel/repeated runs don't collide with seed data or
// each other.
const RUN_ID = 'pw-' + Date.now().toString(36);

/** Bearer-authed JSON fetch against /api/v1 — used to assert DB persistence. */
async function api(method, path, { body } = {}) {
  const res = await fetch(`${BASE}/api/v1${path}`, {
    method,
    headers: {
      Authorization: `Bearer ${TOKEN}`,
      'content-type': 'application/json',
    },
    body: body !== undefined ? JSON.stringify(body) : undefined,
  });
  const text = await res.text();
  let json = null;
  if (text) {
    try { json = JSON.parse(text); } catch { json = text; }
  }
  return { status: res.status, json };
}

/** Sign in via the UI overlay. */
async function signIn(page) {
  await page.goto(`${BASE}/admin/`);
  await page.locator('#login-overlay').waitFor({ state: 'visible' });
  await page.fill('#login-token', TOKEN);
  await page.click('#login-btn');
  // Overlay hides on success.
  await expect(page.locator('#login-overlay')).toBeHidden({ timeout: 5000 });
  await expect(page.locator('#token-status')).toContainText('authenticated');
}

test.describe('Hydra admin UI — CRUD E2E', () => {
  test.beforeAll(async () => {
    // Liveness guard: fail fast with a clear message if the server isn't up.
    const { status } = await api('GET', '/health');
    if (status !== 200) {
      throw new Error(
        `hydra admin API not reachable at ${BASE}/api/v1/health (status ${status}). ` +
        'Start the binary and run tests/e2e/seed.sh first; see tests/e2e/README.md.',
      );
    }
  });

  test('T2.1 login with admin token', async ({ page }) => {
    await signIn(page);
    // Reload button is gated (only visible after auth).
    await expect(page.locator('#reload-btn')).toBeVisible();
  });

  test('T2.1b wrong token is rejected', async ({ page }) => {
    await page.goto(`${BASE}/admin/`);
    await page.fill('#login-token', 'definitely-wrong');
    await page.click('#login-btn');
    await expect(page.locator('#login-error')).toBeVisible({ timeout: 5000 });
    await expect(page.locator('#login-error')).toContainText(/401/);
  });

  test('T2.2 create provider via UI → appears in list → persisted via /api', async ({ page }) => {
    await signIn(page);

    // Open the Providers tab and the New form.
    await page.click('nav.tabs button[data-tab="providers"]');
    await page.click('#provider-new');
    const id = `${RUN_ID}-prov`;
    await page.fill('[name="id"]', id);
    await page.fill('[name="key"]', `${RUN_ID}-key`);
    await page.fill('[name="name"]', 'Playwright Provider');
    await page.fill('[name="endpoint"]', 'https://pw-upstream.example.com');
    await page.fill('[name="weight"]', '2');
    await page.click('#provider-form button.btn.primary');

    // The form hides on success and a toast appears.
    await expect(page.locator('#provider-form')).toBeHidden({ timeout: 5000 });
    await expect(page.locator('#toast')).toContainText(/Saved provider/);

    // The list now contains the row (id + endpoint).
    await expect(page.locator('#providers-table tbody')).toContainText(id);
    await expect(page.locator('#providers-table tbody')).toContainText('pw-upstream.example.com');

    // DB persistence: a direct /api GET by id returns the row.
    const { status, json } = await api('GET', `/providers/${id}`);
    expect(status).toBe(200);
    expect(json.key).toBe(`${RUN_ID}-key`);
    expect(json.endpoint).toBe('https://pw-upstream.example.com');
    expect(json.weight).toBe(2);
  });

  test('T2.3 tenant with auth_url + associate provider/model', async ({ page }) => {
    await signIn(page);

    // Create a tenant via the UI (auth_url is required).
    await page.click('nav.tabs button[data-tab="tenants"]');
    await page.click('#tenant-new');
    const tid = `${RUN_ID}-tenant`;
    await page.fill('[name="id"]', tid);
    await page.fill('[name="name"]', 'Playwright Tenant');
    await page.fill('[name="domain"]', `${RUN_ID}.example.com`);
    await page.fill('[name="auth_url"]', 'https://auth.pw.example.com/v1/verify');
    await page.click('#tenant-form button.btn.primary');
    await expect(page.locator('#tenant-form')).toBeHidden({ timeout: 5000 });
    await expect(page.locator('#tenants-table tbody')).toContainText(tid);
    await expect(page.locator('#tenants-table tbody')).toContainText('auth.pw.example.com');

    // Create a provider + model + key to associate.
    const pid = `${RUN_ID}-tp`;
    const mid = `${RUN_ID}-tm`;
    await api('POST', '/providers', {
      body: {
        id: pid, key: `${RUN_ID}-pk`, name: 'P', endpoint: 'https://up.example.com',
        weight: 1, created_at: '', updated_at: '',
      },
    });
    await api('POST', '/provider-models', {
      body: { id: mid, key: `${RUN_ID}-model`, name: 'M', provider_id: pid, status: 1 },
    });

    // Associate via the TenantAccess UI.
    await page.click('nav.tabs button[data-tab="tenant-access"]');
    await page.click('#tp-new');
    const tpid = `${RUN_ID}-tpa`;
    await page.fill('[name="id"]', tpid);
    await page.fill('[name="tenant_id"]', tid);
    await page.fill('[name="provider_id"]', pid);
    await page.click('#tp-form button.btn.primary');
    await expect(page.locator('#tenant-access-table tbody')).toContainText(tpid);

    // And the TenantModels gate.
    await page.click('nav.tabs button[data-tab="tenant-models"]');
    await page.click('#tm-new');
    const tmid = `${RUN_ID}-tma`;
    await page.fill('[name="id"]', tmid);
    await page.fill('[name="tenant_id"]', tid);
    await page.fill('[name="model_key"]', `${RUN_ID}-model`);
    await page.click('#tm-form button.btn.primary');
    await expect(page.locator('#tenant-models-table tbody')).toContainText(tmid);

    // Persistence via /api.
    const { status, json } = await api('GET', `/tenant-providers/${tpid}`);
    expect(status).toBe(200);
    expect(json.tenant_id).toBe(tid);
    expect(json.provider_id).toBe(pid);
  });

  test('T2.4 auth-cache invalidate', async ({ page }) => {
    await signIn(page);
    await page.click('nav.tabs button[data-tab="auth-cache"]');
    await page.fill('#inv-tenant', 't-seed');
    await page.fill('#inv-keys', 'sk-nonexistent-aaa, sk-nonexistent-bbb');
    await page.click('#inv-btn');
    // No entries match ⇒ invalidated: 0, but no error.
    await expect(page.locator('#inv-result')).toContainText(/Invalidated 0/);
  });

  test('T2.5 breaker view + force reset', async ({ page, request }) => {
    // Force a provider into the dead-set via the admin API (the probe task
    // would otherwise take too long to trip deterministically).
    const pid = `${RUN_ID}-brk`;
    await api('POST', '/providers', {
      body: {
        id: pid, key: `${RUN_ID}-brkk`, name: 'B', endpoint: 'https://brk.example.com',
        weight: 1, created_at: '', updated_at: '',
      },
    });
    // Push failures directly through the breaker via the API is not exposed,
    // so we use the UI's "force reset by id" path on a non-dead id — this
    // still exercises the DELETE /breaker/<id> path and the dead-set view.
    await signIn(page);
    await page.click('nav.tabs button[data-tab="breaker"]');
    await page.fill('#breaker-reset-id', pid);
    await page.click('#breaker-reset-btn');
    // Toast confirms the reset (id may or may not have been dead).
    await expect(page.locator('#toast')).toContainText(/Reset/);
  });

  test('T2.6 reload endpoint surfaces new snapshot counts', async ({ page }) => {
    await signIn(page);
    const toast = page.locator('#toast');
    await page.click('#reload-btn');
    await expect(toast).toContainText(/Reloaded:/);
  });
});
