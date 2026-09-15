# Cori Configuration Schemas

This folder contains JSON Schema definitions for validating Cori's YAML configuration files. These schemas define the structure and constraints for all configuration types used by Cori Store.

## Schema Overview

| Schema | File | Purpose |
|--------|------|---------|
| [CoriDefinition](#coridefinition) | `CoriDefinition.schema.json` | Main configuration file (`cori.yaml`) |
| [SchemaDefinition](#schemadefinition) | `SchemaDefinition.schema.json` | Auto-generated database structure |
| [RulesDefinition](#rulesdefinition) | `RulesDefinition.schema.json` | Tenancy, soft-delete, and validation rules |
| [TypesDefinition](#typesdefinition) | `TypesDefinition.schema.json` | Reusable semantic types for validation |
| [RoleDefinition](#roledefinition) | `RoleDefinition.schema.json` | AI agent roles with table/column permissions |
| [GroupDefinition](#groupdefinition) | `GroupDefinition.schema.json` | Approval groups for human-in-the-loop |
| [AuditEvent](#auditevent) | `AuditEvent.schema.json` | Audit log event structure |

---

## CoriDefinition

**File:** `CoriDefinition.schema.json`  
**Config file:** `cori.yaml`  
**Managed by:** User

The main configuration file that defines database connection, Biscuit token settings, MCP server configuration, dashboard settings, and audit logging. This is the entry point for configuring a Cori instance.

### Sections

| Section | Required | Description |
|---------|----------|-------------|
| `project` | No | Project name identifier |
| `version` | No | Configuration version (e.g., "1.0") |
| `upstream` | **Yes** | PostgreSQL database connection |
| `biscuit` | No | Biscuit token keys (defaults to `keys/` directory) |
| `mcp` | No | MCP server settings (defaults to stdio transport) |
| `dashboard` | No | Admin web UI settings |
| `audit` | No | Audit logging configuration |
| `observability` | No | Metrics, health checks, and tracing |

### Example

```yaml
project: my-saas-app
version: "1.0"

upstream:
  database_url_env: DATABASE_URL
  pool:
    min_connections: 1
    max_connections: 10

biscuit:
  public_key_file: keys/public.key
  private_key_file: keys/private.key

mcp:
  enabled: true
  transport: stdio

dashboard:
  enabled: true
  port: 8080

audit:
  enabled: true
  directory: logs/
```

---

## SchemaDefinition

**File:** `SchemaDefinition.schema.json`  
**Config file:** `schema/schema.yaml`  
**Managed by:** `cori db sync` (auto-generated)

Captures the database structure from introspection. This file is **auto-generated** and should not be edited manually.

### Contents

- **Database metadata**: Engine type (postgres, mysql, etc.) and version
- **Extensions**: Enabled database extensions (e.g., `uuid-ossp`, `pgcrypto`)
- **Enums**: Custom enum types with their values
- **Tables**: Complete table definitions including:
  - Columns with types, nullability, defaults, and constraints
  - Primary keys
  - Foreign keys with referential actions
  - Indexes

### Example

```yaml
version: "1.0.0"
captured_at: "2026-01-08T10:30:00Z"
database:
  engine: postgres
  version: "16.1"
tables:
  - name: customers
    schema: public
    columns:
      - name: id
        type: uuid
        nullable: false
      - name: organization_id
        type: uuid
        nullable: false
      - name: email
        type: string
        nullable: false
    primary_key: [id]
```

---

## RulesDefinition

**File:** `RulesDefinition.schema.json`  
**Config file:** `schema/rules.yaml`  
**Managed by:** User (initialize with `cori rules init`)

Defines tenancy configuration, soft-delete behavior, and column-level validation rules. This is where you specify **how data is segmented per tenant**.

### Key Concepts

| Concept | Description |
|---------|-------------|
| **Direct tenant** | Table has a column holding the tenant ID directly |
| **Inherited tenant** | Table inherits tenant from a parent table via foreign key |
| **Global table** | Shared data across all tenants (no filtering) |
| **Soft delete** | DELETE operations set a column instead of removing rows |

### Example

```yaml
version: "1.0.0"
tables:
  customers:
    description: "Customer accounts"
    tenant: organization_id        # Direct tenant column
    columns:
      email:
        type: email                # Reference to types.yaml
        tags: [pii]
        
  orders:
    tenant:
      via: customer_id             # FK column in this table
      references: customers        # Inherit tenant from customers
    soft_delete:
      column: deleted_at
      deleted_value: "NOW()"
      active_value: null
      
  products:
    global: true                   # No tenant scoping
```

---

## TypesDefinition

**File:** `TypesDefinition.schema.json`  
**Config file:** `schema/types.yaml`  
**Managed by:** User

Defines reusable semantic types for input validation. Types specify regex patterns for validation and can be tagged for categorization (e.g., `pii`, `sensitive`).

### Example

```yaml
version: "1.0.0"
types:
  email:
    description: "Email address"
    pattern: "^[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\\.[a-zA-Z]{2,}$"
    tags: [pii]
    
  phone:
    description: "Phone number (E.164 format)"
    pattern: "^\\+[1-9]\\d{1,14}$"
    tags: [pii]
    
  sku:
    description: "Product SKU code"
    pattern: "^[A-Z]{3}-[0-9]{4}$"
```

---

## RoleDefinition

**File:** `RoleDefinition.schema.json`  
**Config files:** `roles/*.yaml` (one file per role)  
**Managed by:** User

Defines AI agent roles with granular table and column permissions. Roles control **what AI agents can do** when accessing the database via MCP.

### Permission Types

| Permission | Description |
|------------|-------------|
| `readable` | Columns the agent can SELECT (with optional pagination limit) |
| `creatable` | Columns the agent can set on INSERT (with constraints) |
| `updatable` | Columns the agent can modify on UPDATE (with constraints) |
| `deletable` | Whether DELETE is allowed (with optional soft-delete/approval) |

### Readable Configuration

The `readable` field supports multiple formats:

| Format | Example | Description |
|--------|---------|-------------|
| `"*"` | `readable: "*"` | All columns |
| Array | `readable: [id, name, email]` | Specific columns |
| Object | `readable: { columns: [...], max_per_page: 100 }` | Columns with pagination limit |

### Column Constraints

**For creatable columns:**
- `required`: Must provide value on INSERT
- `default`: Auto-set if not provided
- `restrict_to`: Whitelist of allowed values
- `requires_approval`: Human approval needed
- `guidance`: Instructions for AI agents

**For updatable columns:**
- `only_when`: Conditional update rules using `old.*` (current value) and `new.*` (incoming value) syntax
- `requires_approval`: Human approval needed
- `guidance`: Instructions for AI agents

**For deletable:**
- `true`: Hard delete allowed
- `false`: Delete not allowed
- `{ requires_approval: true }`: Delete requires human approval
- `{ soft_delete: true }`: Use soft delete (set column instead of removing)
- `{ requires_approval: true, soft_delete: true }`: Both

### `only_when` Syntax

The `only_when` constraint uses `old.<column>` for current row values and `new.<column>` for incoming values:

| Pattern | Description |
|---------|-------------|
| `new.status: [open, closed]` | Restrict new value to a whitelist |
| `old.status: open, new.status: [closed]` | State transition (open → closed) |
| `new.quantity: { greater_than: old.quantity }` | Increment only |
| `new.notes: { starts_with: old.notes }` | Append only |
| Array of conditions | OR logic (any can match) |

### Comparison Operators

Available in `only_when` conditions:

| Operator | Description |
|----------|-------------|
| `equals` | Must equal value |
| `not_equals` | Must not equal value |
| `greater_than` | Must be greater than value or `old.<column>` |
| `greater_than_or_equal` | Must be greater than or equal |
| `lower_than` | Must be less than value or `old.<column>` |
| `lower_than_or_equal` | Must be less than or equal |
| `not_null` | Must not be null |
| `is_null` | Must be null |
| `in` | Must be one of these values |
| `not_in` | Must not be one of these values |
| `starts_with` | Must start with value or `old.<column>` |

### Example

```yaml
name: support_agent
description: "AI agent for customer support operations"

approvals:
  group: support_managers
  notify_on_pending: true

tables:
  customers:
    readable:
      columns: [id, name, email, plan, created_at]
      max_per_page: 100
    # No creatable/updatable = read-only
    
  tickets:
    readable:
      columns: [id, subject, status, priority, created_at]
      max_per_page: 100
    creatable:
      subject: { required: true }
      description: { required: true }
      customer_id: { required: true }
      priority: { default: medium, restrict_to: [low, medium, high] }
      status: { default: open, restrict_to: [open] }
    updatable:
      status:
        only_when:
          - { old.status: open, new.status: [in_progress, resolved] }
          - { old.status: in_progress, new.status: [open, resolved, escalated] }
          - { old.status: resolved, new.status: open }
      priority:
        only_when: { new.priority: [low, medium, high] }
        requires_approval: true
      description:
        only_when:
          old.status: [open, in_progress]
          new.description: { starts_with: old.description }
    deletable: false

  inventory:
    readable: [id, product_id, quantity, warehouse_id]
    updatable:
      quantity:
        only_when: { new.quantity: { greater_than: old.quantity } }
        guidance: "Quantity can only be increased, never decreased"
```

---

## GroupDefinition

**File:** `GroupDefinition.schema.json`  
**Config files:** `groups/*.yaml` (one file per group)  
**Managed by:** User

Defines approval groups for human-in-the-loop actions. Groups contain members identified by email addresses who can approve sensitive operations.

### Example

```yaml
name: support_managers
description: "Managers who can approve support ticket priority changes"
members:
  - alice.manager@example.com
  - bob.lead@example.com
```

Groups are referenced in role definitions:

```yaml
# In roles/support_agent.yaml
approvals:
  group: support_managers
  notify_on_pending: true
```

---

## Configuration Relationships

```
┌─────────────────────────────────────────────────────────────────────────────┐
│  schema/schema.yaml     │  schema/rules.yaml        │  schema/types.yaml    │
│  (Auto-generated)       │  (User-edited)            │  (User-defined)       │
├─────────────────────────┼───────────────────────────┼───────────────────────┤
│  • Tables & columns     │  • Tenant config          │  • email format       │
│  • Data types           │  • Soft delete            │  • phone pattern      │
│  • Foreign keys         │  • Column validation      │  • Custom types       │
│  • Indexes              │  • Tags (pii, sensitive)  │  • PII tags           │
└─────────────────────────┴───────────────────────────┴───────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│  roles/*.yaml                         │  groups/*.yaml                      │
│  (Access Control)                     │  (Approval Groups)                  │
├───────────────────────────────────────┼─────────────────────────────────────┤
│  • WHO can access what?               │  • WHO can approve?                 │
│  • WHICH columns are readable?        │  • Member email list                │
│  • WHAT can be created/updated?       │  • Referenced by roles              │
│  • WHAT constraints apply?            │                                     │
└───────────────────────────────────────┴─────────────────────────────────────┘
```

---

## AuditEvent

**File:** `AuditEvent.schema.json`  
**Generated by:** Cori audit system  
**Managed by:** Automatic (read-only)

Defines the structure of audit log events written by Cori. These events provide a tamper-evident record of all database operations performed through MCP.

### Event Types

| Type | Description |
|------|-------------|
| `intent_received` | A mutation intent was received from an agent |
| `plan_validated` | The execution plan was validated |
| `policy_checked` | Policy check was performed |
| `approval_required` | Action requires human approval |
| `approved` | Action was approved by a human |
| `action_previewed` | Dry-run preview was executed |
| `action_executed` | Action was executed |
| `verification_failed` | Post-execution verification failed |
| `committed` | Transaction was committed |
| `compensated` | Compensation was applied after failure |
| `failed` | Action failed |

### Required Fields

| Field | Description |
|-------|-------------|
| `event_id` | Unique UUID for this event |
| `occurred_at` | UTC timestamp (RFC3339) |
| `tenant_id` | Tenant the event relates to |
| `intent_id` | The mutation intent this event is part of |
| `principal_id` | The user/agent that initiated the action |
| `step_id` | Step within the plan (or `__intent__` for intent-level) |
| `event_type` | One of the event types above |
| `action` | The action performed (insert, update, delete) |
| `allowed` | Whether the action was allowed by policy |

### Example

```json
{
  "event_id": "550e8400-e29b-41d4-a716-446655440000",
  "occurred_at": "2026-01-18T10:30:00Z",
  "tenant_id": "acme_corp",
  "intent_id": "intent_123",
  "principal_id": "support_agent",
  "step_id": "step_1",
  "event_type": "action_executed",
  "action": "update",
  "resource_kind": "table",
  "resource_id": "tickets",
  "allowed": true,
  "preview": false
}
```

---

## Validation

Use the CLI to validate all configuration files against these schemas:

```bash
cori check
```

This validates:
- `cori.yaml` against `CoriDefinition.schema.json`
- `schema/schema.yaml` against `SchemaDefinition.schema.json`
- `schema/rules.yaml` against `RulesDefinition.schema.json`
- `schema/types.yaml` against `TypesDefinition.schema.json`
- `roles/*.yaml` against `RoleDefinition.schema.json`
- `groups/*.yaml` against `GroupDefinition.schema.json`
- Cross-references (e.g., approval groups exist, type references are valid)

---

## IDE Support

Add these schemas to your IDE for YAML validation and autocompletion:

### VS Code

Add to `.vscode/settings.json`:

```json
{
  "yaml.schemas": {
    "./schemas/CoriDefinition.schema.json": "cori.yaml",
    "./schemas/SchemaDefinition.schema.json": "schema/schema.yaml",
    "./schemas/RulesDefinition.schema.json": "schema/rules.yaml",
    "./schemas/TypesDefinition.schema.json": "schema/types.yaml",
    "./schemas/RoleDefinition.schema.json": "roles/*.yaml",
    "./schemas/GroupDefinition.schema.json": "groups/*.yaml"
  }
}
```
