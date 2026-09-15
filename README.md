<div align="center">


<img src="https://assets.cori.do/cori-logo.png" alt="Cori Logo" width="140" />

### Cori Store

**Governed Postgres data for AI agents and vibe-coded apps.**

[![License: Apache 2.0](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](LICENSE)
[![Built with Rust](https://img.shields.io/badge/Built%20with-Rust-orange.svg)](https://www.rust-lang.org/)

[Quick Start](#-quick-start) • [Why Cori Store](#-the-problem) • [How It Works](#-how-it-works) • [Where It Is Going](#-where-it-is-going)

</div>

---

> **Renamed.** This repository was `cori-kernel`. It is now **Cori Store**, the data
> track of the Cori platform (design: `cori-target/11-datastore.md`, decision D-040).
> "Kernel" only ever refers to the pre-existing codebase. The crates keep their names
> (`cori-core`, `cori-policy`, `cori-mcp`, `cori-adapter-pg`, `cori-audit`, …) and are
> consumed by path from the `cori` workspace (`cori/crates/cori-store`).

---

## 🎯 The Problem

Vibe-coded apps need a database. Today the coding agent either provisions a hosted
Postgres and writes row-level policies it does not understand, or hands raw SQL to an LLM.

- **Multi-tenant data** → Agent for Client A must never see Client B's data
- **Dynamic operations** → LLMs request actions you can't predict
- **Compliance & audit** → You need to know exactly what happened
- **Zero trust** → Traditional app-level security doesn't cut it

**Raw database access for AI is a security nightmare.**

---

## 💡 The Solution

Cori Store makes the database a governed, typed, tenant-scoped object:

- the coding agent can **describe** it (admin tools: schema, rules, roles, migrations),
- the app and its workflows can **use** it (generated typed tools + guarded SQL),
- a non-technical person can **see and edit** it (sheet UI, coming).

```
AI Agent → MCP → Cori Store → Your Postgres
                     ↓
                ✓ Verify token
                ✓ Check permissions
                ✓ Inject tenant isolation
                ✓ Audit everything
```

**Agents discover typed tools. Cori Store protects your data.**

---

## ✨ Key Features (today)

| Feature | Description |
|---------|-------------|
| **📋 Role-Based Access** | Define which tables, columns, and operations each role can access. |
| **🏢 Opt-in Tenant Isolation** | Declare a tenant column (direct or inherited via FK) and every operation is scoped to the caller's tenant. |
| **🤖 MCP Server Built-In** | AI agents discover typed tools, not raw SQL. stdio and HTTP transports. |
| **📏 Policy Validation** | Declarative constraints (`only_when`, `restrict_to`, `required`) enforced at runtime. |
| **✅ Human-in-the-Loop** | Flag sensitive operations for approval before execution. |
| **🔍 Virtual Schema** | Agents only see tables/columns they're allowed to access. |
| **👁️ Full Audit Trail** | Every action logged with who, what, when, and outcome (JSON schema in `schemas/`). |
| **🔐 Biscuit Token Auth** | Current token format: cryptographic tokens with tenant + role claims. **Being replaced by JWT** (see below). |

---

## 🚀 Quick Start

### Install

```sh
curl -fsSL https://cli.cori.do/install.sh | bash
```

### 1. Initialize from Your Database

```sh
cori init --from-db postgres://user:pass@localhost/mydb --project myproject
```

This introspects your database and generates:
- `cori.yaml` — Main configuration
- `keys/` — Biscuit keypair for token signing (until JWT lands)
- `roles/` — Sample role definitions based on your schema
- `groups/` — Sample approval groups
- `schema/schema.yaml` — Auto-generated database schema
- `schema/rules.yaml` — Tenancy, soft-delete, validation rules

### 2. Start the Store

```sh
cd myproject
cori run
# Dashboard on :8080, MCP HTTP server on :3000
```

### 3. Mint a Token

```sh
# Create a role token (uses keys/private.key by default)
cori token mint --role support_agent --output role.token

# Attenuate for a specific tenant
cori token attenuate \
    --base role.token \
    --tenant acme_corp \
    --expires 24h \
    --output agent.token
```

### 4. Connect Your Agent via MCP

Add the store to your AI agent's MCP configuration:

```json
{
  "mcpServers": {
    "cori": {
      "command": "cori",
      "args": ["run", "--stdio", "--config", "cori.yaml", "--token", "agent.token"]
    }
  }
}
```

Your agent now has **typed, safe tools** instead of raw SQL:

```
🔧 Available Tools (8):
   • getCustomer (read)       → Retrieve a customer by ID
   • listCustomers (read)     → List customers with filters
   • getTicket (read)         → Retrieve a ticket by ID
   • listTickets (read)       → List tickets with filters
   • updateTicket (write)     → Update ticket status/priority
   • getOrder (read)          → Retrieve an order by ID
   • listOrders (read)        → List orders with filters
   ...
```

Each tool is:
- **Scoped to the tenant** in the token (no data leaks)
- **Type-checked** with JSON Schema inputs
- **Permission-aware** (only actions the role allows)
- **Constraint-validated** (state machines, required fields enforced)
- **Audited** (every call logged)

Test what tools are available for a token:

```sh
cori tools list --token agent.token --key keys/public.key
```

---

## 🛡️ Audit Logs

Cori Store records **every tool call, SQL query, and approval decision** in both human-readable and structured formats.

- **Console output** (when `audit.stdout` is enabled) prints lines like `[2026-01-10T22:54:10Z] QUERY_EXECUTED role=support_agent tenant=acme_corp action=listCustomers sql="SELECT ..."` and flags approvals (`ApprovalRequested`, `Approved`, `Denied`).
- **JSON log file** is written to `logs/audit.log` inside your project directory. Each line is a compact JSON object with fields such as `event_type`, `role`, `tenant_id`, `action`, `sql`, `approval_id`, `parent_event_id`, and `duration_ms`, making it easy to ship to log processors or parse locally.

The dashboard at `:8080` automatically loads audit logs, with filtering by event type, sortable columns, and pagination for forensic review.

Configure `audit.directory`, `audit.stdout`, and `audit.retention_days` in `cori.yaml`.

In the platform, audit events ship to the platform audit table so one inbox serves workflows and stores.

---

## ✅ Human-in-the-Loop Approvals

When a role has `requires_approval: true` on a column, updates go through the approval workflow:

1. Agent calls the tool (e.g., `updateTicket` with `priority` change)
2. Cori Store returns `"status": "pending_approval"` with an `approval_id`
3. Admin reviews in the Dashboard → **Approvals** tab (or the platform approval inbox)
4. On approval, the operation executes; on rejection, it fails
5. Full audit trail links the approval decision to the original request

```yaml
# In roles/support_agent.yaml
tables:
  tickets:
    updatable:
      priority:
        requires_approval: true  # Goes to the approval inbox for review
```

Approval groups are defined in `groups/*.yaml` and referenced in role definitions.

---

## 🔧 How It Works

### Define Your Tenancy (opt-in)

A store contains only the tables and columns the app needs. Tenancy is declared per
table when you have it; single-company apps declare nothing and get plain tables with
role rules only (D-025).

```yaml
# schema/rules.yaml
version: "1.0.0"

tables:
  customers:
    tenant: organization_id       # Direct tenant column
  orders:
    tenant:
      via: customer_id            # Inherited via FK
      references: customers
  products:
    global: true                  # Shared across all tenants
```

### Define Roles

Specify what each role can do with declarative constraints:

```yaml
# roles/support_agent.yaml
name: support_agent
description: "AI agent for customer support"

approvals:
  group: support_managers         # Approval group for requires_approval

tables:
  customers:
    readable: [id, name, email, plan]
    # No updatable = read-only

  tickets:
    readable: [id, subject, status, priority]
    updatable:
      status:
        only_when:                # State machine constraints
          - old.status: open
            new.status: [in_progress, resolved]
          - old.status: in_progress
            new.status: [open, resolved]
      priority:
        requires_approval: true   # Human must approve

default_page_size: 100
```

### Automatic Tool Generation

Cori Store generates MCP tools from your schema and role permissions:

```
Agent Request:
  tool: listOrders
  arguments: { status: "pending" }

Cori Store Executes:
  SELECT * FROM orders
  WHERE status = 'pending'
  AND customer_org_id = 'acme_corp'  -- injected from token
```

No code changes. No ORM plugins. Just security.

---

## 🤖 MCP Tool Generation

| Role Permission | Generated Tools |
|-----------------|-----------------|
| Table readable | `get{Entity}(id)`, `list{Entities}(filters)` |
| Table has editable columns | `create{Entity}(data)`, `update{Entity}(id, data)` |
| Table deletable | `delete{Entity}(id)` |

Tools include:
- **Typed inputs** — JSON Schema with column types, enums, constraints
- **Filter parameters** — Auto-generated from readable columns
- **Approval flags** — Sensitive fields marked for human-in-the-loop
- **Pagination** — Built-in `limit`/`offset` respecting `default_page_size`

Example generated tool schema:

**Via stdio (Claude Desktop, etc.):**
```json
{
  "name": "updateTicket",
  "description": "Update an existing ticket",
  "inputSchema": {
    "type": "object",
    "properties": {
      "id": { "type": "integer" },
      "status": {
        "type": "string",
        "enum": ["open", "in_progress", "pending_customer", "resolved"]
      },
      "priority": { "type": "string" }
    },
    "required": ["id"]
  },
  "annotations": {
    "requiresApproval": true,
    "dryRunSupported": true
  }
}
```

**Via HTTP (custom agents):**
```sh
# Start HTTP server (default mode)
cori run
# Dashboard at http://localhost:8080
# MCP endpoint at http://localhost:3000

# Call tools via HTTP
curl -X POST http://localhost:3000/mcp \
  -H "Authorization: Bearer <token>" \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc": "2.0", "method": "tools/call", "params": {"name": "listCustomers", "arguments": {}}, "id": 1}'
```

**No raw SQL. Just safe, typed actions.**

---

## 🏗️ Architecture (this repository, today)

```
┌─────────────────────────────────────────────────────────────────┐
│                         cori binary                             │
├─────────────────────────────────────┬───────────────────────────┤
│         MCP Server                  │      Admin Dashboard      │
│  (stdio or http on :3000)           │      (http on :8080)      │
├─────────────────────────────────────┴───────────────────────────┤
│  Tool Generator → Policy Validator → Tenant Inject → Audit      │
│                         ↓                                       │
│              Constraints · Approvals · Permissions              │
├─────────────────────────────────────────────────────────────────┤
│                    Upstream Postgres                            │
└─────────────────────────────────────────────────────────────────┘
```

| Crate | Role | Target status |
|---|---|---|
| `cori-core` | Config types, YAML formats (`schema.yaml`, `rules.yaml`, `types.yaml`, `roles/*.yaml`, `groups/*.yaml`); single source of truth | kept; YAML stays the canonical serialization, loading also fed from the platform DB |
| `cori-policy` | Tenancy rules, role permissions, `only_when` / `restrict_to` / `required` constraints | kept |
| `cori-adapter-pg` | Postgres introspection and execution | kept; the only engine |
| `cori-mcp` | Tool generation per role, dry-run, approvals, stdio and HTTP transports | kept; one endpoint per store with role-filtered tools |
| `cori-audit` | Audit events (JSON schema) | kept; ships to the platform audit table |
| `cori-biscuit` | Biscuit token mint / attenuate / verify | **to be removed** (JWT, D-031) |
| `cori-dashboard` | Askama admin UI, basic auth, role CRUD | **to be replaced** by the sheet UI + platform console |
| `cori-cli` (`cori`) | Standalone binary | **folds into** `cori store …` subcommands of the shared `cori` CLI (D-030) |

---

## 🧭 Where It Is Going

The target is described in `cori-target` (`11-datastore.md`, `15-tech-stack.md`,
`18-repositories.md`, `decisions.md`). Summary:

**Target architecture**

- **One MCP endpoint per store** (D-033): a single URL; the tools listed depend on the
  caller's role. `store_admin` sees DDL, rules, roles and migration tools; other roles see
  only the data tools.
- **Admin tools** (role `store_admin`, builders and the coding agent): `create_table`,
  `alter_table`, `set_rules`, `define_role`, `grant`, `plan_migration` / `apply_migration`
  (every DDL is a migration with a dry-run diff and, by policy, an approval), `import_csv`,
  `seed`. They produce the same YAML this codebase already reads, versioned in the platform
  DB and committable to the app repo (`stores/<name>/`).
- **User tools** (role from the caller's JWT): the generated typed tools plus `query`,
  read-only SQL validated at the AST with `sqlparser-rs` (only `SELECT`, only readable
  tables/columns of the role, tenant predicate injected, row and time caps, function
  allow-list).
- **JWT, not Biscuit** (D-031): the store verifies JWTs directly (JWKS, issuer, audience,
  role and optional tenant claims). Attenuation becomes the IdP's token exchange with short
  TTLs and audience restriction.
- **Minimal schemas, opt-in tenancy** (D-025): nothing is injected into the app schema;
  audit lives in platform tables.
- **N stores per process**, config loaded from the platform DB, one Postgres schema per
  store; on-prem uses the customer's Postgres. The store never needs superuser.
- **Sheet UI** (D-028): a generic React grid over the user tools: tables as tabs, inline
  edit within the role, `only_when` as transition dropdowns, `requires_approval` cells
  pending with a badge, filters, CSV in/out, share-to-role, realtime via SSE. Admins get
  add-column / add-table behind a migration dry-run.
- **CLI**: `cori store init|sync|migrate|token|serve` as subcommands of the shared `cori`
  binary (D-030). `cori dev` runs a local store against a local Postgres.

**Roadmap (own track, runs in parallel with the platform phases)**

| Phase | Scope | Exit |
|---|---|---|
| **S0 — Extract** | Repository renamed `cori-store` (done); config loading as a library API fed from the platform DB, YAML kept canonical; JWKS-based JWT verification replacing `cori-biscuit`; N stores per process | the demo schema served through the platform gateway with an SSO JWT |
| **S1 — Admin MCP and migrations** | DDL tools, migration plan/apply with dry-run and approval, rules/roles editing tools, `import_csv`, agent skill `cori-design-store` | the coding agent creates the `support-tickets` store from a prompt, with tenancy and roles, reviewed as a migration |
| **S2 — Guarded SQL and `store` step** | `query` tool with AST validation, row/time caps; `store` step kind in the compiler and worker; generated store client | a workflow reads through `query` with the tenant predicate proven; a forbidden column is rejected at the AST |
| **S3 — Sheet UI** | grid, inline edit with constraints, pending approvals, filters, share-to-role, CSV, realtime | a support lead edits ticket priorities in the grid, through `requires_approval`, visible in the audit |
| **S4 — Scale and on-prem** | backups/PITR per store, per-store pools, customer-side Postgres, read replicas for `query`, column-level encryption for `pii` | ongoing |

**Already built on these crates in the `cori` workspace** (`cori/crates/cori-store`,
see `cori-target/IMPLEMENTATION-STATUS.md`): a Postgres database per store, migrations
from `stores/<name>/migrations/*.sql`, tools generated per role, guarded `query`, admin
DDL tools recorded as migrations, one endpoint per store (`POST /v1/stores/{name}/mcp`),
approvals bridged to the platform inbox, text primary keys, a first sheet UI, and
`cori store list|tools|query`. Those pieces move into this repository as S0–S2 land;
`cori-store-svc` (N stores for one org, sheet UI API) stays in `cori`.

**Gaps this repository still has to fill**: DDL tools, guarded free SQL, JWT verification,
config from the platform DB instead of files only, multi-store hosting, realtime, sheet UI,
CSV import/export, migrations with history.

**Not planned**: engines other than Postgres, a BI layer.

---

## 🆚 Why Not Just...

| Alternative | Problem |
|-------------|---------|
| **Native Postgres RLS** | Requires session variables; no standard token format; no MCP |
| **OPA / Cerbos / Cedar** | Extra service to deploy; latency; policy sprawl |
| **API Gateway** | Doesn't understand database operations; can't inject row-level predicates |
| **LangChain SQL Agent** | Generates raw SQL; no tenant isolation |

**Cori Store is purpose-built for the AI-agent-to-database use case.**

---

## 📊 Current Status

> **Alpha** — MCP server, token system, policy validation, and approvals work. The track
> above (S0–S4) is the path to the platform.

| Component | Status |
|-----------|--------|
| MCP tool generation | ✅ Working |
| Tenant isolation | ✅ Working |
| Policy validation | ✅ Working |
| Human-in-the-loop approvals | ✅ Working |
| Audit logging | ✅ Working |
| Biscuit token auth | ✅ Working (to be replaced by JWT, S0) |
| Admin dashboard | ✅ Working (to be replaced by sheet UI + console, S3) |
| JWT verification (JWKS) | ⏳ S0 |
| Config from platform DB, N stores per process | ⏳ S0 |
| Admin DDL / migration tools | ⏳ S1 (prototype in `cori/crates/cori-store`) |
| Guarded `query` (AST-validated SQL) | ⏳ S2 (prototype in `cori/crates/cori-store`) |
| Sheet UI | ⏳ S3 (prototype in `cori`) |

---

## 📖 Documentation

- **[AGENTS.md](AGENTS.md)** — portable context for coding agents: architecture, config formats, target
- **[examples/demo/](examples/demo/)** — Working demo with Docker Compose
- **[docs/COMMANDS.md](docs/COMMANDS.md)** — CLI command reference
- **[schemas/](schemas/)** — JSON schemas for configuration files
- **`../cori-target/11-datastore.md`** — the Cori Store design and roadmap

---

## 🔨 Building from Source

```sh
git clone https://github.com/cori-do/cori-store.git
cd cori-store
cargo install --path crates/cori-cli
```

Requires Rust (stable). See [rust-lang.org](https://www.rust-lang.org/tools/install) for installation.

---

## 🤝 Contributing

We'd love your help! Here's how:

- ⭐ **Star the repo** — It helps others find us
- 🐛 **Report bugs** — Open an issue
- 💡 **Suggest features** — Tell us your use case

---

## 📜 License

Apache 2.0 today. The platform target (`cori-target/18-repositories.md`, D-038) places
`cori-store` under FSL like the other public repositories; that switch is a separate
decision and has not been applied here.

---

<div align="center">

**Cori Store: Because AI agents shouldn't have `sudo` on your database.**

[Get Started](#-quick-start) • [Star on GitHub ⭐](https://github.com/cori-do/cori-store)

</div>
