/* Hydra admin UI — vanilla JS, no build step (design §14.2).
 *
 * Embeds via include_dir! at compile time. Talks same-origin to /api/v1/* with
 * `Authorization: Bearer <admin-token>`. The token is held in memory only
 * (never persisted to localStorage) and cleared on sign-out/tab close.
 *
 * Pages: Providers / Models / Keys / Tenants / TenantAccess / TenantModels /
 * LimitRoles / AuthCache / Breaker / Health.
 */
"use strict";

// ----- state ----------------------------------------------------------------
let TOKEN = null;            // admin bearer token (in-memory only)
const API = "/api/v1";

// ----- small DOM helpers ----------------------------------------------------
const $ = (sel, root = document) => root.querySelector(sel);
const $$ = (sel, root = document) => Array.from(root.querySelectorAll(sel));
function el(tag, attrs = {}, ...children) {
  const node = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (k === "class") node.className = v;
    else if (k === "text") node.textContent = v;
    else if (k === "html") node.innerHTML = v;
    else if (k.startsWith("on") && typeof v === "function") {
      node.addEventListener(k.slice(2), v);
    } else if (v !== null && v !== undefined) {
      node.setAttribute(k, v);
    }
  }
  for (const c of children) {
    if (c === null || c === undefined) continue;
    node.appendChild(typeof c === "string" ? document.createTextNode(c) : c);
  }
  return node;
}
function esc(s) {
  if (s === null || s === undefined) return "";
  return String(s)
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;");
}

// ----- toast ----------------------------------------------------------------
let toastTimer = null;
function toast(msg, kind = "ok") {
  const t = $("#toast");
  t.className = `toast ${kind}`;
  t.textContent = msg;
  if (toastTimer) clearTimeout(toastTimer);
  toastTimer = setTimeout(() => t.classList.add("hidden"), 3500);
}

// ----- HTTP wrapper ---------------------------------------------------------
async function api(method, path, { body, query } = {}) {
  if (!TOKEN) throw new Error("not authenticated");
  const headers = { Authorization: `Bearer ${TOKEN}` };
  let url = API + path;
  if (query) url += query;
  const opts = { method, headers };
  if (body !== undefined) {
    headers["Content-Type"] = "application/json";
    opts.body = typeof body === "string" ? body : JSON.stringify(body);
  }
  const resp = await fetch(url, opts);
  const text = await resp.text();
  let json = null;
  if (text) {
    try { json = JSON.parse(text); } catch { json = text; }
  }
  if (!resp.ok) {
    const code = json?.error?.code || resp.status;
    const message = json?.error?.message || (typeof json === "string" ? json : resp.statusText);
    const err = new Error(`${resp.status} ${code}: ${message}`);
    err.status = resp.status;
    err.code = code;
    err.body = json;
    throw err;
  }
  return json;
}

// ----- login ----------------------------------------------------------------
function showLogin() {
  TOKEN = null;
  $("#login-overlay").classList.remove("hidden");
  $("#token-status").textContent = "not authenticated";
  $("#token-status").className = "token-status bad";
  $("#login-token").value = "";
  $("#login-token").focus();
}
function hideLogin() {
  $("#login-overlay").classList.add("hidden");
  $("#token-status").textContent = "authenticated";
  $("#token-status").className = "token-status ok";
}
async function tryLogin(token) {
  TOKEN = token;
  try {
    await api("GET", "/health");
    hideLogin();
    sessionStorage.setItem("hydra-admin-ok", "1");
    await refreshAll();
    toast("Signed in");
  } catch (e) {
    TOKEN = null;
    const err = $("#login-error");
    err.textContent = `Authentication failed: ${e.message}`;
    err.classList.remove("hidden");
  }
}

// ----- tabs -----------------------------------------------------------------
function setupTabs() {
  $$("#tabs button").forEach((btn) => {
    btn.addEventListener("click", () => {
      $$("#tabs button").forEach((b) => b.classList.remove("active"));
      btn.classList.add("active");
      const tab = btn.dataset.tab;
      $$("section.panel").forEach((p) => p.classList.toggle("active", p.dataset.panel === tab));
      refreshTab(tab);
    });
  });
}

// ----- generic table renderers ---------------------------------------------
function clearTbody(tableSel) {
  $(`${tableSel} tbody`).innerHTML = "";
}
function setCount(sel, n) {
  const node = $(sel);
  if (node) node.textContent = `(${n})`;
}
function row(...cells) {
  const tr = document.createElement("tr");
  for (const c of cells) {
    const td = document.createElement("td");
    if (typeof c === "string" || typeof c === "number") td.textContent = c;
    else if (c instanceof Node) td.appendChild(c);
    else if (c && c.html !== undefined) td.innerHTML = c.html;
    tr.appendChild(td);
  }
  return tr;
}
function statusPill(status) {
  // -1 = probe-offline, 0 = manual offline, 1 = online (design §4.2)
  const map = { 1: ["ok", "online"], 0: ["warn", "offline"], [-1]: ["err", "dead(probe)"] };
  const [cls, label] = map[status] || ["", String(status)];
  return el("span", { class: `pill ${cls}`, text: label });
}
function boolPill(v) {
  return el("span", { class: `pill ${v ? "ok" : ""}`, text: v ? "true" : "false" });
}

// ----- forms ----------------------------------------------------------------
function field(labelText, name, opts = {}) {
  const input = el(opts.tag || "input", {
    name,
    type: opts.type || "text",
    placeholder: opts.placeholder || "",
    value: opts.value !== undefined ? opts.value : "",
  });
  if (opts.tag === "textarea") input.textContent = opts.value || "";
  const lbl = el("label", { class: "field" },
    el("span", { text: labelText }),
    input,
  );
  if (opts.tip) lbl.appendChild(el("div", { class: "muted", style: "font-size:11px;margin-top:2px", text: opts.tip }));
  return lbl;
}
function fieldValue(container, name) {
  const node = $(`[name="${name}"]`, container);
  if (!node) return "";
  if (node.type === "checkbox") return node.checked;
  return node.value;
}
function buildForm(title, fields, onSubmit) {
  const wrap = el("div", { class: "banner" });
  const form = el("div", { class: "field-row", style: "flex-wrap:wrap" });
  for (const f of fields) form.appendChild(f);
  const actions = el("div", { class: "right", style: "margin-top:6px" },
    el("button", { class: "btn primary", text: "Save", onClick: () => onSubmit(wrap) }),
    " ",
    el("button", { class: "btn", text: "Cancel", onClick: () => wrap.classList.add("hidden") }),
  );
  wrap.appendChild(el("strong", { text: title }));
  wrap.appendChild(form);
  wrap.appendChild(actions);
  return wrap;
}
function jsonObjectFrom(container, spec) {
  // spec: [{ name, type?: 'int'|'bool'|'json'|'opt'|'optint' }]
  const out = {};
  for (const s of spec) {
    let v = fieldValue(container, s.name);
    if (s.type === "int") out[s.name] = v === "" ? 0 : parseInt(v, 10);
    else if (s.type === "optint") out[s.name] = v === "" ? null : parseInt(v, 10);
    else if (s.type === "bool") out[s.name] = !!v;
    else if (s.type === "opt") out[s.name] = v === "" ? null : v;
    else if (s.type === "json") out[s.name] = v === "" ? null : JSON.parse(v);
    else out[s.name] = v;
  }
  return out;
}

// ===========================================================================
// Providers
// ===========================================================================
async function loadProviders() {
  let rows = [];
  try { rows = await api("GET", "/providers") || []; } catch (e) { return toast(e.message, "err"); }
  setCount("#providers-count", rows.length);
  clearTbody("#providers-table");
  const tbody = $("#providers-table tbody");
  for (const p of rows) {
    tbody.appendChild(row(
      esc(p.id), esc(p.key), esc(p.name),
      el("span", { class: "mono", text: p.endpoint }),
      el("span", { class: "num", text: p.weight }),
      el("span", { class: "muted mono", text: p.created_at }),
      el("span", { class: "muted mono", text: p.updated_at }),
      actionCell(() => editProvider(p), () => deleteProvider(p.id)),
    ));
  }
}
function newProviderForm() {
  const f = buildProviderForm({}, false);
  $("#provider-form").innerHTML = "";
  $("#provider-form").appendChild(f);
  f.classList.remove("hidden");
}
function editProvider(p) {
  const f = buildProviderForm(p, true);
  $("#provider-form").innerHTML = "";
  $("#provider-form").appendChild(f);
  f.classList.remove("hidden");
}
function buildProviderForm(p, isEdit) {
  return buildForm(isEdit ? `Edit provider ${p.id}` : "New provider", [
    field("id", "id", { value: p.id || "", placeholder: "auto if blank" }),
    field("key", "key", { value: p.key || "" }),
    field("name", "name", { value: p.name || "" }),
    field("endpoint", "endpoint", { value: p.endpoint || "", placeholder: "https://api.openai.com" }),
    field("weight", "weight", { type: "int", value: p.weight ?? 1, tip: "0 = soft-disabled (§7.2)" }),
  ], async (container) => {
    try {
      const body = jsonObjectFrom(container, [
        { name: "id" }, { name: "key" }, { name: "name" }, { name: "endpoint" },
        { name: "weight", type: "int" },
      ]);
      body.created_at = p.created_at || "";
      body.updated_at = "";
      if (isEdit) await api("PUT", `/providers/${p.id}`, { body });
      else await api("POST", "/providers", { body });
      container.classList.add("hidden");
      toast("Saved provider");
      await loadProviders();
    } catch (e) { toast(e.message, "err"); }
  });
}
async function deleteProvider(id) {
  if (!confirm(`Delete provider ${id}? Cascades to models/keys.`)) return;
  try { await api("DELETE", `/providers/${id}`); toast("Deleted"); await loadProviders(); }
  catch (e) { toast(e.message, "err"); }
}

// ===========================================================================
// Provider Models
// ===========================================================================
async function loadModels() {
  let rows = [];
  try { rows = await api("GET", "/provider-models") || []; } catch (e) { return toast(e.message, "err"); }
  setCount("#models-count", rows.length);
  clearTbody("#models-table");
  const tbody = $("#models-table tbody");
  for (const m of rows) {
    tbody.appendChild(row(
      esc(m.id), esc(m.key), esc(m.name), esc(m.provider_id),
      statusPill(m.status),
      actionCell(null, () => deleteModel(m.id)),
    ));
  }
}
function newModelForm() {
  const f = buildForm("New provider model", [
    field("id", "id", { placeholder: "auto if blank" }),
    field("key (model_key)", "key", { placeholder: "gpt-4" }),
    field("name", "name"),
    field("provider_id", "provider_id"),
    field("status", "status", { type: "int", value: "1", tip: "1=online, 0=manual offline, -1=probe-offline" }),
  ], async (container) => {
    try {
      const body = jsonObjectFrom(container, [
        { name: "id" }, { name: "key" }, { name: "name" },
        { name: "provider_id" }, { name: "status", type: "int" },
      ]);
      await api("POST", "/provider-models", { body });
      container.classList.add("hidden");
      toast("Saved model");
      await loadModels();
    } catch (e) { toast(e.message, "err"); }
  });
  $("#model-form").innerHTML = "";
  $("#model-form").appendChild(f);
}
async function deleteModel(id) {
  if (!confirm(`Delete model ${id}?`)) return;
  try { await api("DELETE", `/provider-models/${id}`); toast("Deleted"); await loadModels(); }
  catch (e) { toast(e.message, "err"); }
}

// ===========================================================================
// Provider Keys (masked by default; reveal=1 opt-in)
// ===========================================================================
async function loadKeys() {
  const reveal = $("#keys-reveal").checked ? "?reveal=1" : "";
  let rows = [];
  try { rows = await api("GET", "/provider-keys", { query: reveal }) || []; } catch (e) { return toast(e.message, "err"); }
  setCount("#keys-count", rows.length);
  clearTbody("#keys-table");
  const tbody = $("#keys-table tbody");
  for (const k of rows) {
    tbody.appendChild(row(
      esc(k.id), esc(k.provider_id),
      el("span", { class: "mono", text: k.api_key }),
      el("span", { class: "muted mono", text: k.created_at }),
      actionCell(null, () => deleteKey(k.id)),
    ));
  }
}
function newKeyForm() {
  const f = buildForm("New provider key", [
    field("id", "id", { placeholder: "auto if blank" }),
    field("provider_id", "provider_id"),
    field("api_key", "api_key", { placeholder: "sk-..." }),
  ], async (container) => {
    try {
      const body = jsonObjectFrom(container, [
        { name: "id" }, { name: "provider_id" }, { name: "api_key" },
      ]);
      body.created_at = "";
      await api("POST", "/provider-keys", { body });
      container.classList.add("hidden");
      toast("Saved key (returned masked)");
      await loadKeys();
    } catch (e) { toast(e.message, "err"); }
  });
  $("#key-form").innerHTML = "";
  $("#key-form").appendChild(f);
}
async function deleteKey(id) {
  if (!confirm(`Delete key ${id}?`)) return;
  try { await api("DELETE", `/provider-keys/${id}`); toast("Deleted"); await loadKeys(); }
  catch (e) { toast(e.message, "err"); }
}

// ===========================================================================
// Tenants (auth_url mandatory, design §11.1)
// ===========================================================================
async function loadTenants() {
  let rows = [];
  try { rows = await api("GET", "/tenants") || []; } catch (e) { return toast(e.message, "err"); }
  setCount("#tenants-count", rows.length);
  clearTbody("#tenants-table");
  const tbody = $("#tenants-table tbody");
  for (const t of rows) {
    tbody.appendChild(row(
      esc(t.id), esc(t.name), el("span", { class: "mono", text: t.domain }),
      el("span", { class: "mono", text: t.auth_url }),
      el("span", { class: "muted mono", text: certDesc(t) }),
      boolPill(t.enabled),
      el("span", { class: "muted mono", text: t.created_at }),
      el("span", { class: "muted mono", text: t.updated_at }),
      actionCell(() => editTenant(t), () => deleteTenant(t.id)),
    ));
  }
}
function certDesc(t) {
  if (!t.cert_file && !t.cert_key) return "(plain)";
  return `${t.cert_file || "-"} / ${t.cert_key || "-"}`;
}
function newTenantForm() {
  const f = buildTenantForm({}, false);
  $("#tenant-form").innerHTML = "";
  $("#tenant-form").appendChild(f);
}
function editTenant(t) {
  const f = buildTenantForm(t, true);
  $("#tenant-form").innerHTML = "";
  $("#tenant-form").appendChild(f);
}
function buildTenantForm(t, isEdit) {
  return buildForm(isEdit ? `Edit tenant ${t.id}` : "New tenant", [
    field("id", "id", { value: t.id || "", placeholder: "auto if blank" }),
    field("name", "name", { value: t.name || "" }),
    field("domain", "domain", { value: t.domain || "", placeholder: "acme.com (lowercased)" }),
    field("auth_url (required)", "auth_url", {
      value: t.auth_url || "",
      placeholder: "https://auth.acme.com/v1/verify",
      tip: "Mandatory (§11.1). Empty ⇒ all requests 401.",
    }),
    field("cert_file (path)", "cert_file", { value: t.cert_file || "", tip: "absolute or relative to data/" }),
    field("cert_key (path)", "cert_key", { value: t.cert_key || "" }),
    field("enabled", "enabled", { type: "bool", value: t.enabled ?? true }),
  ], async (container) => {
    try {
      const body = jsonObjectFrom(container, [
        { name: "id" }, { name: "name" }, { name: "domain" }, { name: "auth_url" },
        { name: "cert_file", type: "opt" }, { name: "cert_key", type: "opt" },
        { name: "enabled", type: "bool" },
      ]);
      body.created_at = t.created_at || "";
      body.updated_at = "";
      if (isEdit) await api("PUT", `/tenants/${t.id}`, { body });
      else await api("POST", "/tenants", { body });
      container.classList.add("hidden");
      toast("Saved tenant");
      await loadTenants();
    } catch (e) { toast(e.message, "err"); }
  });
}
async function deleteTenant(id) {
  if (!confirm(`Delete tenant ${id}?`)) return;
  try { await api("DELETE", `/tenants/${id}`); toast("Deleted"); await loadTenants(); }
  catch (e) { toast(e.message, "err"); }
}

// ===========================================================================
// TenantAccess (tenant ↔ provider)
// ===========================================================================
async function loadTenantAccess() {
  let rows = [];
  try { rows = await api("GET", "/tenant-providers") || []; } catch (e) { return toast(e.message, "err"); }
  setCount("#tenant-access-count", rows.length);
  clearTbody("#tenant-access-table");
  const tbody = $("#tenant-access-table tbody");
  for (const tp of rows) {
    tbody.appendChild(row(
      esc(tp.id), esc(tp.tenant_id), esc(tp.provider_id),
      actionCell(null, () => deleteTP(tp.id)),
    ));
  }
}
function newTPForm() {
  const f = buildForm("New tenant↔provider", [
    field("id", "id", { placeholder: "auto if blank" }),
    field("tenant_id", "tenant_id"),
    field("provider_id", "provider_id"),
  ], async (container) => {
    try {
      const body = jsonObjectFrom(container, [
        { name: "id" }, { name: "tenant_id" }, { name: "provider_id" },
      ]);
      await api("POST", "/tenant-providers", { body });
      container.classList.add("hidden");
      toast("Saved");
      await loadTenantAccess();
    } catch (e) { toast(e.message, "err"); }
  });
  $("#tp-form").innerHTML = "";
  $("#tp-form").appendChild(f);
}
async function deleteTP(id) {
  if (!confirm(`Delete tenant-provider ${id}?`)) return;
  try { await api("DELETE", `/tenant-providers/${id}`); toast("Deleted"); await loadTenantAccess(); }
  catch (e) { toast(e.message, "err"); }
}

// ===========================================================================
// TenantModels (access gate, design §7.1)
// ===========================================================================
async function loadTenantModels() {
  let rows = [];
  try { rows = await api("GET", "/tenant-models") || []; } catch (e) { return toast(e.message, "err"); }
  setCount("#tenant-models-count", rows.length);
  clearTbody("#tenant-models-table");
  const tbody = $("#tenant-models-table tbody");
  for (const tm of rows) {
    tbody.appendChild(row(
      esc(tm.id), esc(tm.tenant_id), esc(tm.model_key),
      actionCell(null, () => deleteTM(tm.id)),
    ));
  }
}
function newTMForm() {
  const f = buildForm("New tenant↔model", [
    field("id", "id", { placeholder: "auto if blank" }),
    field("tenant_id", "tenant_id"),
    field("model_key", "model_key", { placeholder: "gpt-4" }),
  ], async (container) => {
    try {
      const body = jsonObjectFrom(container, [
        { name: "id" }, { name: "tenant_id" }, { name: "model_key" },
      ]);
      await api("POST", "/tenant-models", { body });
      container.classList.add("hidden");
      toast("Saved");
      await loadTenantModels();
    } catch (e) { toast(e.message, "err"); }
  });
  $("#tm-form").innerHTML = "";
  $("#tm-form").appendChild(f);
}
async function deleteTM(id) {
  if (!confirm(`Delete tenant-model ${id}?`)) return;
  try { await api("DELETE", `/tenant-models/${id}`); toast("Deleted"); await loadTenantModels(); }
  catch (e) { toast(e.message, "err"); }
}

// ===========================================================================
// LimitRoles
// ===========================================================================
async function loadLimitRoles() {
  let rows = [];
  try { rows = await api("GET", "/limit-roles") || []; } catch (e) { return toast(e.message, "err"); }
  setCount("#limit-roles-count", rows.length);
  clearTbody("#limit-roles-table");
  const tbody = $("#limit-roles-table tbody");
  for (const r of rows) {
    tbody.appendChild(row(
      esc(r.id), esc(r.name),
      esc(r.matching_tenant || "*"), esc(r.matching_key || "*"),
      esc(r.matching_model || "*"), esc(r.matching_provider || "*"),
      el("span", { class: "num", text: r.limit_count ?? "-" }),
      el("span", { class: "num", text: r.limit_token ?? "-" }),
      esc(r.window), boolPill(r.enabled),
      actionCell(() => editLR(r), () => deleteLR(r.id)),
    ));
  }
}
function newLRForm() {
  const f = buildLRForm({}, false);
  $("#lr-form").innerHTML = "";
  $("#lr-form").appendChild(f);
}
function editLR(r) {
  const f = buildLRForm(r, true);
  $("#lr-form").innerHTML = "";
  $("#lr-form").appendChild(f);
}
function buildLRForm(r, isEdit) {
  return buildForm(isEdit ? `Edit limit role ${r.id}` : "New limit role", [
    field("id", "id", { value: r.id || "", placeholder: "auto if blank" }),
    field("name", "name", { value: r.name || "" }),
    field("matching_tenant", "matching_tenant", { value: r.matching_tenant || "", tip: "blank = match all" }),
    field("matching_key", "matching_key", { value: r.matching_key || "" }),
    field("matching_model", "matching_model", { value: r.matching_model || "" }),
    field("matching_provider", "matching_provider", { value: r.matching_provider || "" }),
    field("limit_count", "limit_count", { type: "optint", value: r.limit_count ?? "" }),
    field("limit_token", "limit_token", { type: "optint", value: r.limit_token ?? "" }),
    field("window", "window", { value: r.window || "m", tip: "m / h / d" }),
    field("enabled", "enabled", { type: "bool", value: r.enabled ?? true }),
  ], async (container) => {
    try {
      const body = jsonObjectFrom(container, [
        { name: "id" }, { name: "name" },
        { name: "matching_tenant", type: "opt" }, { name: "matching_key", type: "opt" },
        { name: "matching_model", type: "opt" }, { name: "matching_provider", type: "opt" },
        { name: "limit_count", type: "optint" }, { name: "limit_token", type: "optint" },
        { name: "window" }, { name: "enabled", type: "bool" },
      ]);
      body.created_at = r.created_at || "";
      if (isEdit) await api("PUT", `/limit-roles/${r.id}`, { body });
      else await api("POST", "/limit-roles", { body });
      container.classList.add("hidden");
      toast("Saved role");
      await loadLimitRoles();
    } catch (e) { toast(e.message, "err"); }
  });
}
async function deleteLR(id) {
  if (!confirm(`Delete limit role ${id}?`)) return;
  try { await api("DELETE", `/limit-roles/${id}`); toast("Deleted"); await loadLimitRoles(); }
  catch (e) { toast(e.message, "err"); }
}

// ===========================================================================
// AuthCache invalidation
// ===========================================================================
async function doInvalidate() {
  const tenant = $("#inv-tenant").value.trim();
  const keysRaw = $("#inv-keys").value.trim();
  const keys = keysRaw ? keysRaw.split(",").map((s) => s.trim()).filter(Boolean) : null;
  if (!tenant && !keys) { toast("Provide tenant_id and/or api_keys", "err"); return; }
  const body = {};
  if (tenant) body.tenant_id = tenant;
  if (keys) body.api_keys = keys;
  try {
    const r = await api("DELETE", "/auth/cache", { body });
    $("#inv-result").classList.remove("hidden");
    $("#inv-result").className = "banner ok";
    $("#inv-result").textContent = `Invalidated ${r.invalidated} entr${r.invalidated === 1 ? "y" : "ies"}.`;
    toast(`Invalidated ${r.invalidated}`);
  } catch (e) {
    $("#inv-result").classList.remove("hidden");
    $("#inv-result").className = "banner err";
    $("#inv-result").textContent = e.message;
  }
}

// ===========================================================================
// Breaker
// ===========================================================================
async function loadBreaker() {
  let body = { dead: [] };
  try { body = await api("GET", "/breaker"); } catch (e) { return toast(e.message, "err"); }
  clearTbody("#breaker-table");
  const tbody = $("#breaker-table tbody");
  // Show dead rows + a note that live providers are filtered out by routing.
  if (!body.dead || body.dead.length === 0) {
    tbody.appendChild(row(
      { html: '<td colspan="3" class="muted">No dead providers (all candidates selectable).</td>' },
    ));
  } else {
    for (const pid of body.dead) {
      tbody.appendChild(row(
        esc(pid),
        el("span", { class: "pill dead", text: "DEAD" }),
        actionCell(null, () => resetBreaker(pid)),
      ));
    }
  }
}
async function resetBreaker(id) {
  try { await api("DELETE", `/breaker/${encodeURIComponent(id)}`); toast(`Reset ${id}`); await loadBreaker(); }
  catch (e) { toast(e.message, "err"); }
}
async function resetBreakerById() {
  const id = $("#breaker-reset-id").value.trim();
  if (!id) { toast("provider id required", "err"); return; }
  await resetBreaker(id);
}

// ===========================================================================
// Health
// ===========================================================================
async function loadHealth() {
  try {
    const h = await api("GET", "/health");
    $("#health-out").textContent = JSON.stringify(h, null, 2);
  } catch (e) {
    $("#health-out").textContent = `error: ${e.message}`;
  }
}

// ===========================================================================
// Wiring
// ===========================================================================
function actionCell(onEdit, onDelete) {
  const td = el("td", { class: "nowrap" });
  if (onEdit) td.appendChild(el("button", { class: "btn small", text: "edit", onClick: onEdit }));
  if (onEdit && onDelete) td.appendChild(document.createTextNode(" "));
  if (onDelete) td.appendChild(el("button", { class: "btn small danger", text: "delete", onClick: onDelete }));
  return td;
}

async function refreshAll() {
  await Promise.all([
    loadProviders(), loadModels(), loadKeys(), loadTenants(),
    loadTenantAccess(), loadTenantModels(), loadLimitRoles(),
    loadBreaker(), loadHealth(),
  ]);
}
function refreshTab(tab) {
  switch (tab) {
    case "providers": return loadProviders();
    case "models": return loadModels();
    case "keys": return loadKeys();
    case "tenants": return loadTenants();
    case "tenant-access": return loadTenantAccess();
    case "tenant-models": return loadTenantModels();
    case "limit-roles": return loadLimitRoles();
    case "breaker": return loadBreaker();
    case "health": return loadHealth();
  }
}

function wireEvents() {
  $("#login-btn").addEventListener("click", () => tryLogin($("#login-token").value));
  $("#login-token").addEventListener("keydown", (e) => {
    if (e.key === "Enter") tryLogin($("#login-token").value);
  });
  $("#logout-btn").addEventListener("click", () => { sessionStorage.removeItem("hydra-admin-ok"); showLogin(); });
  $("#reload-btn").addEventListener("click", async () => {
    try { const r = await api("POST", "/reload", { body: {} }); toast(`Reloaded: ${r.providers} providers, ${r.tenants} tenants`); await refreshAll(); }
    catch (e) { toast(e.message, "err"); }
  });

  $("#provider-new").addEventListener("click", newProviderForm);
  $("#model-new").addEventListener("click", newModelForm);
  $("#key-new").addEventListener("click", newKeyForm);
  $("#keys-reveal").addEventListener("change", loadKeys);
  $("#tenant-new").addEventListener("click", newTenantForm);
  $("#tp-new").addEventListener("click", newTPForm);
  $("#tm-new").addEventListener("click", newTMForm);
  $("#lr-new").addEventListener("click", newLRForm);
  $("#inv-btn").addEventListener("click", doInvalidate);
  $("#breaker-refresh").addEventListener("click", loadBreaker);
  $("#breaker-reset-btn").addEventListener("click", resetBreakerById);
  $("#health-refresh").addEventListener("click", loadHealth);
}

document.addEventListener("DOMContentLoaded", () => {
  setupTabs();
  wireEvents();
  showLogin();
});
