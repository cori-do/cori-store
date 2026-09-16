//! Tool execution engine.
//!
//! This module handles the actual execution of MCP tools, including:
//! - Mapping tool calls to SQL queries
//! - Applying RLS predicates
//! - Dry-run execution
//! - Constraint validation
//! - Result formatting
//! - Audit logging

use crate::approval::{ApprovalManager, ApprovalPendingResponse};
use crate::protocol::{CallToolOptions, DryRunResult, ToolContent, ToolDefinition};
use crate::schema::DatabaseSchema;
use cori_audit::AuditLogger;
use cori_core::RoleDefinition;
use cori_core::config::rules_definition::{RulesDefinition, TenantConfig};
use cori_policy::{OperationType, ToolValidator, ValidationRequest};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use std::sync::Arc;
use std::time::Instant;

/// Result of a tool execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionResult {
    /// Whether the execution was successful.
    pub success: bool,
    /// The result content.
    pub content: Vec<ToolContent>,
    /// Whether this was a dry-run.
    #[serde(rename = "isDryRun")]
    pub is_dry_run: bool,
    /// Error message if failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// SQL query that was executed (for audit logging).
    #[serde(skip)]
    pub executed_sql: Option<String>,
    /// State before mutation (for audit logging).
    #[serde(skip)]
    pub before_state: Option<Value>,
    /// State after mutation (for audit logging).
    #[serde(skip)]
    pub after_state: Option<Value>,
}

impl ExecutionResult {
    /// Create a successful result with JSON content.
    pub fn success_json(value: Value) -> Self {
        Self {
            success: true,
            content: vec![ToolContent::Json { json: value }],
            is_dry_run: false,
            error: None,
            executed_sql: None,
            before_state: None,
            after_state: None,
        }
    }

    /// Create a successful result with JSON content and SQL.
    pub fn success_with_sql(value: Value, sql: impl Into<String>) -> Self {
        Self {
            success: true,
            content: vec![ToolContent::Json { json: value }],
            is_dry_run: false,
            error: None,
            executed_sql: Some(sql.into()),
            before_state: None,
            after_state: None,
        }
    }

    /// Create a successful mutation result with before/after state for audit.
    pub fn mutation_success(
        value: Value,
        sql: impl Into<String>,
        before_state: Option<Value>,
        after_state: Option<Value>,
    ) -> Self {
        Self {
            success: true,
            content: vec![ToolContent::Json { json: value }],
            is_dry_run: false,
            error: None,
            executed_sql: Some(sql.into()),
            before_state,
            after_state,
        }
    }

    /// Create a successful dry-run result.
    pub fn dry_run(result: DryRunResult) -> Self {
        Self {
            success: true,
            content: vec![ToolContent::Json {
                json: serde_json::to_value(result).unwrap_or_default(),
            }],
            is_dry_run: true,
            error: None,
            executed_sql: None,
            before_state: None,
            after_state: None,
        }
    }

    /// Create an error result.
    pub fn error(message: impl Into<String>) -> Self {
        let msg = message.into();
        Self {
            success: false,
            content: vec![ToolContent::Text { text: msg.clone() }],
            is_dry_run: false,
            error: Some(msg),
            executed_sql: None,
            before_state: None,
            after_state: None,
        }
    }

    /// Create a pending approval result.
    pub fn pending_approval(response: ApprovalPendingResponse) -> Self {
        Self {
            success: true,
            content: vec![ToolContent::Json {
                json: serde_json::to_value(response).unwrap_or_default(),
            }],
            is_dry_run: false,
            error: None,
            executed_sql: None,
            before_state: None,
            after_state: None,
        }
    }
}

/// Context for tool execution.
#[derive(Debug, Clone)]
pub struct ExecutionContext {
    /// Tenant ID from token.
    pub tenant_id: String,
    /// Role name from token.
    pub role: String,
    /// Connection ID (for auditing).
    pub connection_id: Option<String>,
}

/// The tool executor handles running tools against the database.
pub struct ToolExecutor {
    /// Role definition for permission checks.
    role: RoleDefinition,
    /// Approval manager for human-in-the-loop.
    approval_manager: Arc<ApprovalManager>,
    /// Database connection pool.
    pool: Option<PgPool>,
    /// Rules definition for tenant configuration.
    rules: Option<RulesDefinition>,
    /// Database schema for primary key lookup.
    schema: Option<DatabaseSchema>,
    /// Audit logger for tracking tool calls and queries.
    audit_logger: Option<Arc<AuditLogger>>,
}

impl ToolExecutor {
    /// Create a new tool executor.
    pub fn new(role: RoleDefinition, approval_manager: Arc<ApprovalManager>) -> Self {
        Self {
            role,
            approval_manager,
            pool: None,
            rules: None,
            schema: None,
            audit_logger: None,
        }
    }

    /// Set the audit logger.
    pub fn with_audit_logger(mut self, logger: Arc<AuditLogger>) -> Self {
        self.audit_logger = Some(logger);
        self
    }

    /// Set the database connection pool.
    pub fn with_pool(mut self, pool: PgPool) -> Self {
        self.pool = Some(pool);
        self
    }

    /// Set the database URL and create a pool.
    pub async fn with_database_url(mut self, url: impl Into<String>) -> Result<Self, sqlx::Error> {
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect(&url.into())
            .await?;
        self.pool = Some(pool);
        Ok(self)
    }

    /// Set the rules definition for tenant configuration.
    pub fn with_rules(mut self, rules: RulesDefinition) -> Self {
        self.rules = Some(rules);
        self
    }

    /// Set the database schema for primary key lookup.
    pub fn with_schema(mut self, schema: DatabaseSchema) -> Self {
        self.schema = Some(schema);
        self
    }

    fn tenant_column_for_table(&self, table: &str) -> Option<String> {
        let rules = self.rules.as_ref()?;
        let table_rules = rules.get_table_rules(table)?;

        // Check if global table
        if table_rules.global.unwrap_or(false) {
            return None;
        }

        // Get tenant config
        match &table_rules.tenant {
            Some(TenantConfig::Direct(column)) => Some(column.clone()),
            Some(TenantConfig::Inherited(_)) => {
                // For inherited tenancy, we need to JOIN through FK
                // For now, return None (handled separately in query building)
                None
            }
            None => None,
        }
    }

    fn validate_tenant_for_table(&self, table: &str, tenant_id: &str) -> Result<(), String> {
        let tenant_column = self.tenant_column_for_table(table);
        if tenant_column.is_none() {
            // Global / unscoped table or inherited tenancy: tenant not required at this level.
            return Ok(());
        }

        if tenant_id == "unknown" || tenant_id.trim().is_empty() {
            return Err(format!(
                "Missing tenant id for tenant-scoped table '{}'. Provide a tenant-scoped Biscuit token (with a tenant(...) attenuation block) via --token or CORI_TOKEN.",
                table
            ));
        }

        // No type validation here - database will validate
        Ok(())
    }

    /// Convert a JSON value to a SQL literal for use in queries.
    fn value_to_sql_literal(&self, value: &Value) -> String {
        if let Some(s) = value.as_str() {
            format!("'{}'", s.replace("'", "''"))
        } else if let Some(n) = value.as_i64() {
            n.to_string()
        } else if let Some(f) = value.as_f64() {
            f.to_string()
        } else if let Some(b) = value.as_bool() {
            b.to_string()
        } else if value.is_null() {
            "NULL".to_string()
        } else {
            format!("'{}'", value.to_string().replace("'", "''"))
        }
    }

    /// Get the primary key columns for a table from schema.
    /// Returns empty vector if no primary key is defined in the schema.
    fn get_primary_key_columns(&self, table: &str) -> Vec<String> {
        if let Some(schema) = &self.schema
            && let Some(table_schema) = schema.get_table(table) {
                return table_schema.primary_key.clone();
            }
        // No schema or table not found - return empty (no PK)
        Vec::new()
    }

    /// Get the referenced table for a foreign key column from schema.
    /// Returns None if no FK is found or schema is not available.
    fn get_fk_referenced_table(&self, table: &str, column: &str) -> Option<String> {
        let schema = self.schema.as_ref()?;
        let table_schema = schema.get_table(table)?;
        
        for fk in &table_schema.foreign_keys {
            for fk_col in &fk.columns {
                if fk_col.column == column {
                    return Some(fk_col.foreign_table.clone());
                }
            }
        }
        None
    }

    /// Check if a role has old.* constraints for a table that require fetching current row.
    fn role_has_old_constraints(&self, table: &str) -> bool {
        if let Some(perms) = self.role.tables.get(table) {
            // Check if any updatable column has only_when constraints with old.* conditions
            for col_name in perms.updatable.column_names() {
                if let Some(constraints) = perms.updatable.get_constraints(col_name)
                    && let Some(only_when) = &constraints.only_when
                        && only_when.has_old_conditions() {
                            return true;
                        }
            }
        }
        false
    }

    /// Fetch the current row from database for UPDATE validation.
    async fn fetch_current_row(
        &self,
        table: &str,
        arguments: &Value,
        context: &ExecutionContext,
    ) -> Result<Option<Value>, String> {
        let pool = match &self.pool {
            Some(p) => p,
            None => return Err("Database connection not configured".to_string()),
        };

        let pk_columns = self.get_primary_key_columns(table);
        if pk_columns.is_empty() {
            return Err(format!("No primary key defined for table '{}'", table));
        }

        // Extract PK values from arguments
        let mut pk_conditions: Vec<String> = Vec::new();
        for pk_col in &pk_columns {
            match arguments.get(pk_col) {
                Some(v) => {
                    if let Some(n) = v.as_i64() {
                        pk_conditions.push(format!("{} = {}", pk_col, n));
                    } else if let Some(s) = v.as_str() {
                        pk_conditions.push(format!("{} = '{}'", pk_col, s.replace("'", "''")));
                    } else {
                        return Err(format!(
                            "Unsupported primary key type for column '{}'",
                            pk_col
                        ));
                    }
                }
                None => return Err(format!("Missing primary key field: {}", pk_col)),
            }
        }

        // Add tenant condition if applicable
        let tenant_column = self.tenant_column_for_table(table);
        let tenant_condition = tenant_column.as_ref().map(|tc| {
            format!("{} = {}", tc, sql_string_literal(&context.tenant_id))
        });

        let query = if let Some(tc) = tenant_condition {
            format!(
                "SELECT * FROM {} WHERE {} AND {}",
                table,
                pk_conditions.join(" AND "),
                tc
            )
        } else {
            format!(
                "SELECT * FROM {} WHERE {}",
                table,
                pk_conditions.join(" AND ")
            )
        };

        tracing::debug!("Fetching current row for validation: {}", query);

        match sqlx::query(&query).fetch_optional(pool).await {
            Ok(Some(row)) => {
                let data = row_to_json(&row, &[]);
                Ok(Some(data))
            }
            Ok(None) => Ok(None),
            Err(e) => Err(format!("Database error: {}", e)),
        }
    }

    /// Fetch current row values by ID for snapshot validation.
    /// Used when creating approval requests for updates/deletes.
    async fn fetch_row_snapshot(
        &self,
        table: &str,
        pk_value: i64,
        tenant_id: &str,
    ) -> Option<Value> {
        let pool = self.pool.as_ref()?;
        let pk_column = self.get_primary_key_columns(table).into_iter().next()?;
        let tenant_column = self.tenant_column_for_table(table);

        let query = if let Some(tc) = &tenant_column {
            // With tenant scoping
            format!(
                "SELECT * FROM {} WHERE {} = {} AND {} = {}",
                table, pk_column, pk_value, tc, sql_string_literal(tenant_id)
            )
        } else {
            format!("SELECT * FROM {} WHERE {} = {}", table, pk_column, pk_value)
        };

        match sqlx::query(&query).fetch_optional(pool).await {
            Ok(Some(row)) => Some(row_to_json(&row, &[])),
            _ => None,
        }
    }

    /// Execute a tool call.
    pub async fn execute(
        &self,
        tool: &ToolDefinition,
        arguments: Value,
        options: &CallToolOptions,
        context: &ExecutionContext,
    ) -> ExecutionResult {
        let start_time = Instant::now();

        // Generate correlation ID for this workflow
        let correlation_id = uuid::Uuid::new_v4().to_string();

        // Track event IDs for hierarchical linking
        let approval_requested_event_id: Option<uuid::Uuid> = None;

        // 0. Parse tool name to determine operation and table
        let operation = self.parse_tool_operation(&tool.name);

        // 1. Create validator and validate the request against role and rules
        let (op_type, table) = match &operation {
            ToolOperation::Get { table } => (OperationType::Get, table.as_str()),
            ToolOperation::List { table } => (OperationType::List, table.as_str()),
            ToolOperation::Create { table } => (OperationType::Create, table.as_str()),
            ToolOperation::Update { table } => (OperationType::Update, table.as_str()),
            ToolOperation::Delete { table } => (OperationType::Delete, table.as_str()),
            ToolOperation::Custom { name } => {
                // Custom actions are not validated through the standard flow
                return self
                    .execute_custom(name, &arguments, options, context)
                    .await;
            }
        };

        // Get primary key columns from schema (supports composite keys)
        let pk_columns = self.get_primary_key_columns(table);
        let pk_column_refs: Vec<&str> = pk_columns.iter().map(|s| s.as_str()).collect();

        // For UPDATE operations with old.* constraints, we need to fetch the current row first
        let current_row: Option<Value> = if op_type == OperationType::Update {
            // Check if role has old.* constraints that need current row
            if self.role_has_old_constraints(table) {
                // Fetch current row from database
                match self.fetch_current_row(table, &arguments, context).await {
                    Ok(Some(row)) => Some(row),
                    Ok(None) => {
                        return ExecutionResult::error("Record not found or access denied");
                    }
                    Err(e) => {
                        return ExecutionResult::error(format!(
                            "Failed to fetch current row for validation: {}",
                            e
                        ));
                    }
                }
            } else {
                None
            }
        } else {
            None
        };

        // Build the validation request
        let validation_request = ValidationRequest {
            operation: op_type,
            table,
            arguments: &arguments,
            tenant_id: &context.tenant_id,
            role_name: &context.role,
            current_row: current_row.as_ref(),
            primary_key_columns: Some(pk_column_refs.clone()),
        };

        // Create validator with role and optionally rules
        let mut validator = ToolValidator::new(&self.role);
        if let Some(rules) = &self.rules {
            validator = validator.with_rules(rules);
        }

        // Validate the request (checks permissions and constraints)
        if let Err(e) = validator.validate(&validation_request) {
            // Log authorization denied
            if let Some(logger) = &self.audit_logger {
                let _ = logger
                    .log_authorization_denied(
                        &context.role,
                        &context.tenant_id,
                        &tool.name,
                        &e.to_string(),
                        Some(&correlation_id),
                    )
                    .await;
            }
            return ExecutionResult::error(e.to_string());
        }

        // 2. Check if approval is required (role-level requires_approval)
        if let Some(approval_fields) = validator.requires_approval(&validation_request)
            && !options.dry_run {
                // Determine if this is an update/delete operation that needs a snapshot
                let request = match &operation {
                    ToolOperation::Update { table } | ToolOperation::Delete { table } => {
                        // For updates/deletes, snapshot current values for validation
                        // Get the actual primary key column name from schema
                        let pk_columns = self.get_primary_key_columns(table);
                        let pk_value = pk_columns
                            .first()
                            .and_then(|pk_col| arguments.get(pk_col))
                            .and_then(|v| v.as_i64())
                            .unwrap_or(0);

                        if pk_value > 0 {
                            // Fetch current row values
                            let snapshot = self
                                .fetch_row_snapshot(table, pk_value, &context.tenant_id)
                                .await;

                            if let Some(original_values) = snapshot {
                                // Create request with snapshot
                                self.approval_manager.create_request_with_snapshot(
                                    &tool.name,
                                    arguments.clone(),
                                    approval_fields,
                                    &context.tenant_id,
                                    &context.role,
                                    table,
                                    json!(pk_value),
                                    original_values,
                                )
                            } else {
                                // Record not found, create regular request
                                self.approval_manager.create_request(
                                    &tool.name,
                                    arguments.clone(),
                                    approval_fields,
                                    &context.tenant_id,
                                    &context.role,
                                )
                            }
                        } else {
                            // No valid ID, create regular request
                            self.approval_manager.create_request(
                                &tool.name,
                                arguments.clone(),
                                approval_fields,
                                &context.tenant_id,
                                &context.role,
                            )
                        }
                    }
                    _ => {
                        // For other operations (create, etc.), no snapshot needed
                        self.approval_manager.create_request(
                            &tool.name,
                            arguments.clone(),
                            approval_fields,
                            &context.tenant_id,
                            &context.role,
                        )
                    }
                };

                // Log approval request with full context (arguments and original state)
                if let Some(logger) = &self.audit_logger
                    && let Ok(event_id) = logger
                        .log_approval_requested_with_context(
                            &context.role,
                            &context.tenant_id,
                            &tool.name,
                            &request.id,
                            request.arguments.clone(),
                            request.original_values.clone(),
                            Some(&correlation_id),
                        )
                        .await
                    {
                        // approval_requested_event_id = Some(event_id);

                        // Update the approval request with audit IDs for hierarchical linking
                        if let Err(e) = self.approval_manager.update_audit_ids(
                            &request.id,
                            event_id,
                            correlation_id.clone(),
                        ) {
                            tracing::warn!(error = %e, "Failed to update approval request with audit IDs");
                        }
                    }
                return ExecutionResult::pending_approval(ApprovalPendingResponse::from(&request));
            }
            // In dry-run mode, continue to show what would happen

        // 3. Validate arguments against tool schema constraints
        if let Err(e) = self.validate_arguments(tool, &arguments) {
            return ExecutionResult::error(e);
        }

        // 4. Execute based on operation type
        let result = match operation {
            ToolOperation::Get { table } => {
                self.execute_get(&table, &arguments, options, context).await
            }
            ToolOperation::List { table } => {
                self.execute_list(&table, &arguments, options, context)
                    .await
            }
            ToolOperation::Create { table } => {
                self.execute_create(&table, &arguments, options, context)
                    .await
            }
            ToolOperation::Update { table } => {
                self.execute_update(&table, &arguments, options, context)
                    .await
            }
            ToolOperation::Delete { table } => {
                self.execute_delete(&table, &arguments, options, context)
                    .await
            }
            ToolOperation::Custom { name } => {
                self.execute_custom(&name, &arguments, options, context)
                    .await
            }
        };

        // Log execution result
        let duration_ms = start_time.elapsed().as_millis() as u64;
        if let Some(logger) = &self.audit_logger {
            if result.success {
                // Extract row count from result if available
                let row_count = result
                    .content
                    .first()
                    .and_then(|c| match c {
                        ToolContent::Json { json } => json.get("count").and_then(|v| v.as_u64()),
                        _ => None,
                    })
                    .unwrap_or(0);

                let sql = result.executed_sql.as_deref().unwrap_or("");

                // Check if this is a mutation with before/after state
                if result.before_state.is_some() || result.after_state.is_some() {
                    // Log mutation with diff
                    let _ = logger
                        .log_mutation_executed(
                            &context.role,
                            &context.tenant_id,
                            &tool.name,
                            sql,
                            row_count,
                            duration_ms,
                            arguments.clone(),
                            result.before_state.clone(),
                            result.after_state.clone(),
                            approval_requested_event_id,
                            Some(&correlation_id),
                        )
                        .await;
                } else {
                    // Log regular query
                    let _ = logger
                        .log_query_executed(
                            &context.role,
                            &context.tenant_id,
                            &tool.name,
                            sql,
                            row_count,
                            duration_ms,
                            approval_requested_event_id,
                            Some(&correlation_id),
                        )
                        .await;
                }
            } else if let Some(error) = &result.error {
                let _ = logger
                    .log_query_failed(
                        &context.role,
                        &context.tenant_id,
                        &tool.name,
                        None,
                        error,
                        approval_requested_event_id,
                        Some(&correlation_id),
                    )
                    .await;
            }
        }

        result
    }

    /// Execute an approved request after validating that data hasn't changed.
    ///
    /// This method should be called by the dashboard after an approval is granted.
    /// It validates that the original snapshot matches current DB values before executing.
    pub async fn execute_approved(
        &self,
        approval: &crate::approval::ApprovalRequest,
        context: &ExecutionContext,
    ) -> ExecutionResult {
        // Validate snapshot if present (for updates/deletes)
        if let (Some(table), Some(pk), Some(original_values)) = (
            &approval.target_table,
            &approval.target_pk,
            &approval.original_values,
        ) {
            let pk_value = pk.as_i64().unwrap_or(0);
            if pk_value > 0 {
                // Fetch current values
                let current_values = self
                    .fetch_row_snapshot(table, pk_value, &context.tenant_id)
                    .await;

                match current_values {
                    Some(current) => {
                        // Compare with original snapshot
                        if &current != original_values {
                            return ExecutionResult::error(format!(
                                "Data has changed since approval was requested. Original: {}, Current: {}. Please create a new approval request.",
                                original_values, current
                            ));
                        }
                    }
                    None => {
                        return ExecutionResult::error(
                            "Record not found or access denied. The record may have been deleted.",
                        );
                    }
                }
            }
        }

        // Find the tool definition for this request
        // We need to generate a temporary tool definition for execution
        let tool = ToolDefinition {
            name: approval.tool_name.clone(),
            description: Some(format!("Executing approved request: {}", approval.id)),
            input_schema: json!({
                "type": "object",
                "properties": {},
            }),
            annotations: None,
        };

        // Execute without dry-run and without re-checking approval
        let options = CallToolOptions {
            dry_run: false,
            ..Default::default()
        };

        // Parse operation and execute directly (bypass approval check)
        let operation = self.parse_tool_operation(&tool.name);
        let arguments = approval.arguments.clone();

        match operation {
            ToolOperation::Get { table } => {
                self.execute_get(&table, &arguments, &options, context)
                    .await
            }
            ToolOperation::List { table } => {
                self.execute_list(&table, &arguments, &options, context)
                    .await
            }
            ToolOperation::Create { table } => {
                self.execute_create(&table, &arguments, &options, context)
                    .await
            }
            ToolOperation::Update { table } => {
                self.execute_update(&table, &arguments, &options, context)
                    .await
            }
            ToolOperation::Delete { table } => {
                self.execute_delete(&table, &arguments, &options, context)
                    .await
            }
            ToolOperation::Custom { name } => {
                self.execute_custom(&name, &arguments, &options, context)
                    .await
            }
        }
    }

    /// Validate arguments against tool constraints.
    fn validate_arguments(&self, tool: &ToolDefinition, arguments: &Value) -> Result<(), String> {
        let schema = &tool.input_schema;

        // Check required fields
        if let Some(required) = schema["required"].as_array() {
            for req in required {
                if let Some(field) = req.as_str()
                    && arguments.get(field).is_none() {
                        return Err(format!("Missing required field: {}", field));
                    }
            }
        }

        // Check enum constraints
        if let Some(props) = schema["properties"].as_object() {
            for (field, prop_schema) in props {
                if let Some(value) = arguments.get(field) {
                    // Check enum
                    if let Some(allowed) = prop_schema["enum"].as_array() {
                        let allowed_values: Vec<&Value> = allowed.iter().collect();
                        if !allowed_values.contains(&value) {
                            return Err(format!(
                                "Invalid value for '{}': {:?}. Allowed: {:?}",
                                field, value, allowed
                            ));
                        }
                    }

                    // Check type
                    if let Some(expected_type) = prop_schema["type"].as_str()
                        && !self.check_type(value, expected_type) {
                            return Err(format!(
                                "Invalid type for '{}': expected {}, got {:?}",
                                field, expected_type, value
                            ));
                        }

                    // Check min/max
                    if let Some(min) = prop_schema["minimum"].as_f64()
                        && let Some(v) = value.as_f64()
                            && v < min {
                                return Err(format!(
                                    "Value for '{}' must be at least {}",
                                    field, min
                                ));
                            }
                    if let Some(max) = prop_schema["maximum"].as_f64()
                        && let Some(v) = value.as_f64()
                            && v > max {
                                return Err(format!(
                                    "Value for '{}' must be at most {}",
                                    field, max
                                ));
                            }

                    // Check pattern
                    if let Some(pattern) = prop_schema["pattern"].as_str()
                        && let Some(s) = value.as_str()
                            && let Ok(re) = regex::Regex::new(pattern)
                                && !re.is_match(s) {
                                    return Err(format!(
                                        "Value for '{}' does not match pattern: {}",
                                        field, pattern
                                    ));
                                }
                }
            }
        }

        Ok(())
    }

    /// Check if a value matches an expected type.
    fn check_type(&self, value: &Value, expected: &str) -> bool {
        match expected {
            "string" => value.is_string(),
            "integer" => value.is_i64() || value.is_u64(),
            "number" => value.is_number(),
            "boolean" => value.is_boolean(),
            "object" => value.is_object(),
            "array" => value.is_array(),
            "null" => value.is_null(),
            _ => true,
        }
    }

    /// Parse tool name to determine the operation.
    fn parse_tool_operation(&self, name: &str) -> ToolOperation {
        // Standard patterns: get{Entity}, list{Entities}, create{Entity}, update{Entity}, delete{Entity}
        // Database tables are typically plural (e.g., customers, tickets, orders)
        if let Some(entity) = name.strip_prefix("get") {
            // getCustomer -> customers table
            ToolOperation::Get {
                table: pluralize(&snake_case(entity)),
            }
        } else if let Some(entity) = name.strip_prefix("list") {
            // listCustomers -> customers table (already plural)
            ToolOperation::List {
                table: snake_case(entity),
            }
        } else if let Some(entity) = name.strip_prefix("create") {
            // createCustomer -> customers table
            ToolOperation::Create {
                table: pluralize(&snake_case(entity)),
            }
        } else if let Some(entity) = name.strip_prefix("update") {
            // updateCustomer -> customers table
            ToolOperation::Update {
                table: pluralize(&snake_case(entity)),
            }
        } else if let Some(entity) = name.strip_prefix("delete") {
            // deleteCustomer -> customers table
            ToolOperation::Delete {
                table: pluralize(&snake_case(entity)),
            }
        } else {
            ToolOperation::Custom {
                name: name.to_string(),
            }
        }
    }

    /// Execute a GET operation.
    async fn execute_get(
        &self,
        table: &str,
        arguments: &Value,
        options: &CallToolOptions,
        context: &ExecutionContext,
    ) -> ExecutionResult {
        if let Err(e) = self.validate_tenant_for_table(table, &context.tenant_id) {
            return ExecutionResult::error(e);
        }

        // Get primary key columns from schema (supports composite keys)
        let pk_columns = self.get_primary_key_columns(table);

        // Cannot execute GET without a primary key
        if pk_columns.is_empty() {
            return ExecutionResult::error(format!(
                "Cannot get record from table '{}': no primary key defined in schema",
                table
            ));
        }

        let tenant_column = self.tenant_column_for_table(table);

        // Extract all PK values from arguments
        let mut pk_values: Vec<(&str, i64)> = Vec::new();
        for pk_col in &pk_columns {
            match arguments.get(pk_col) {
                Some(v) => {
                    let val = v.as_i64().unwrap_or(0);
                    pk_values.push((pk_col, val));
                }
                None => {
                    return ExecutionResult::error(format!(
                        "Missing required primary key field: {}",
                        pk_col
                    ));
                }
            }
        }

        if options.dry_run {
            let pk_conditions: Vec<String> = pk_columns
                .iter()
                .enumerate()
                .map(|(i, col)| format!("{} = ${}", col, i + 1))
                .collect();
            let where_clause = if let Some(tc) = &tenant_column {
                format!(
                    " WHERE {} AND {} = ${}",
                    pk_conditions.join(" AND "),
                    tc,
                    pk_columns.len() + 1
                )
            } else {
                format!(" WHERE {}", pk_conditions.join(" AND "))
            };
            return ExecutionResult::dry_run(DryRunResult {
                dry_run: true,
                would_affect: json!({
                    table: { "select": 1 }
                }),
                preview: Some(json!({
                    "query": format!("SELECT * FROM {}{}", table, where_clause),
                    "pk_columns": pk_columns,
                    "tenant_id": context.tenant_id
                })),
            });
        }

        // Execute actual query
        let pool = match &self.pool {
            Some(p) => p,
            None => {
                return ExecutionResult::error("Database connection not configured");
            }
        };

        // Get the readable columns for this table (or use * if not specified)
        let columns = self.get_readable_columns(table);
        let column_list = if columns.is_empty() {
            "*".to_string()
        } else {
            columns.join(", ")
        };

        // Build primary key conditions
        let pk_conditions: Vec<String> = pk_values
            .iter()
            .map(|(col, val)| format!("{} = {}", col, val))
            .collect();

        // Build optional tenant condition - embed directly since it comes from trusted token
        let tenant_condition = tenant_column.as_ref().map(|tc| {
            format!("{} = {}", tc, sql_string_literal(&context.tenant_id))
        });

        let query = if let Some(tc) = tenant_condition {
            format!(
                "SELECT {} FROM {} WHERE {} AND {}",
                column_list,
                table,
                pk_conditions.join(" AND "),
                tc
            )
        } else {
            format!(
                "SELECT {} FROM {} WHERE {}",
                column_list,
                table,
                pk_conditions.join(" AND ")
            )
        };

        tracing::debug!("Executing GET query: {}", query);

        let result = sqlx::query(&query).fetch_optional(pool).await;

        match result {
            Ok(Some(row)) => {
                let data = row_to_json(&row, &columns);
                ExecutionResult::success_with_sql(data, &query)
            }
            Ok(None) => ExecutionResult::success_with_sql(
                json!({
                    "data": null,
                    "message": "Record not found"
                }),
                &query,
            ),
            Err(e) => ExecutionResult::error(format!("Database error: {}", e)),
        }
    }

    /// Execute a LIST operation.
    async fn execute_list(
        &self,
        table: &str,
        arguments: &Value,
        options: &CallToolOptions,
        context: &ExecutionContext,
    ) -> ExecutionResult {
        if let Err(e) = self.validate_tenant_for_table(table, &context.tenant_id) {
            return ExecutionResult::error(e);
        }
        let limit = arguments
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(50);
        let offset = arguments
            .get("offset")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

        // Apply max rows limit from role (per-table max_per_page)
        let max_rows = self.role.get_max_per_page(table).unwrap_or(1000);
        let effective_limit = limit.min(max_rows);
        let tenant_column = self.tenant_column_for_table(table);

        if options.dry_run {
            let where_clause = if let Some(tc) = &tenant_column {
                format!(" WHERE {} = $1", tc)
            } else {
                "".to_string()
            };
            return ExecutionResult::dry_run(DryRunResult {
                dry_run: true,
                would_affect: json!({
                    table: { "select": "unknown" }
                }),
                preview: Some(json!({
                    "query": format!("SELECT * FROM {}{} LIMIT {} OFFSET {}", table, where_clause, effective_limit, offset),
                    "params": if tenant_column.is_some() { json!([context.tenant_id]) } else { json!([]) }
                })),
            });
        }

        // Execute actual query
        let pool = match &self.pool {
            Some(p) => p,
            None => {
                return ExecutionResult::error("Database connection not configured");
            }
        };

        // Get the readable columns for this table (or use * if not specified)
        let columns = self.get_readable_columns(table);
        let column_list = if columns.is_empty() {
            "*".to_string()
        } else {
            columns.join(", ")
        };

        // Build filter conditions from arguments (excluding limit/offset)
        // Tenant ID comes from the trusted token, so we can safely embed it.
        let mut conditions: Vec<String> = Vec::new();
        if let Some(tc) = &tenant_column {
            let tenant_condition = format!("{} = {}", tc, sql_string_literal(&context.tenant_id));
            conditions.push(tenant_condition);
        }

        // Add user-provided filter conditions: equality on columns the role can read, nothing else. An unreadable
        // column would leak its values one guess at a time; an unknown key or a non-scalar value is an error rather
        // than a filter silently dropped.
        let filterable = self.filterable_columns(table);
        let empty_map = serde_json::Map::new();
        let args_map = arguments.as_object().unwrap_or(&empty_map);
        for (key, value) in args_map {
            if key == "limit" || key == "offset" || key == "dryRun" {
                continue;
            }
            if !filterable.iter().any(|c| c == key) {
                return ExecutionResult::error(format!(
                    "Unknown filter '{}' for {}: filter by one of the readable columns {:?} (equality only), plus limit and offset",
                    key, table, filterable
                ));
            }
            if let Some(s) = value.as_str() {
                conditions.push(format!("\"{}\" = {}", key, sql_string_literal(s)));
            } else if let Some(n) = value.as_i64() {
                conditions.push(format!("\"{}\" = {}", key, n));
            } else if let Some(f) = value.as_f64() {
                conditions.push(format!("\"{}\" = {}", key, f));
            } else if let Some(b) = value.as_bool() {
                conditions.push(format!("\"{}\" = {}", key, b));
            } else if value.is_null() {
                conditions.push(format!("\"{}\" IS NULL", key));
            } else {
                return ExecutionResult::error(format!(
                    "Filter '{}' must be a string, number, boolean or null (equality only)",
                    key
                ));
            }
        }

        let query = if conditions.is_empty() {
            format!(
                "SELECT {} FROM {} LIMIT {} OFFSET {}",
                column_list, table, effective_limit, offset
            )
        } else {
            format!(
                "SELECT {} FROM {} WHERE {} LIMIT {} OFFSET {}",
                column_list,
                table,
                conditions.join(" AND "),
                effective_limit,
                offset
            )
        };

        tracing::info!("Executing LIST query: {}", query);

        let result = sqlx::query(&query).fetch_all(pool).await;

        match result {
            Ok(rows) => {
                let data: Vec<Value> = rows.iter().map(|r| row_to_json(r, &columns)).collect();
                ExecutionResult::success_with_sql(
                    json!({
                        "data": data,
                        "count": data.len(),
                        "limit": effective_limit,
                        "offset": offset
                    }),
                    &query,
                )
            }
            Err(e) => ExecutionResult::error(format!("Database error: {}", e)),
        }
    }

    /// Columns a list may filter on: the role's readable columns (all schema columns when the role reads every column).
    fn filterable_columns(&self, table: &str) -> Vec<String> {
        let listed = self.get_readable_columns(table);
        if !listed.is_empty() {
            return listed;
        }
        if !self.role.can_read(table) {
            return Vec::new();
        }
        self.schema
            .as_ref()
            .and_then(|s| s.get_table(table))
            .map(|t| t.columns.iter().map(|c| c.name.clone()).collect())
            .unwrap_or_default()
    }

    /// Get readable columns for a table from role.
    fn get_readable_columns(&self, table: &str) -> Vec<String> {
        self.role
            .get_readable_columns(table)
            .and_then(|cols| cols.as_list())
            .map(|cols| cols.to_vec())
            .unwrap_or_default()
    }

    /// Execute a CREATE operation.
    async fn execute_create(
        &self,
        table: &str,
        arguments: &Value,
        options: &CallToolOptions,
        context: &ExecutionContext,
    ) -> ExecutionResult {
        if let Err(e) = self.validate_tenant_for_table(table, &context.tenant_id) {
            return ExecutionResult::error(e);
        }
        let tenant_column = self.tenant_column_for_table(table);
        if options.dry_run {
            return ExecutionResult::dry_run(DryRunResult {
                dry_run: true,
                would_affect: json!({
                    table: { "insert": 1 }
                }),
                preview: Some(json!({
                    "operation": "INSERT",
                    "table": table,
                    "data": arguments,
                    "tenant_id": context.tenant_id
                })),
            });
        }

        // Build INSERT statement from arguments
        let empty_map = serde_json::Map::new();
        let obj = arguments.as_object().unwrap_or(&empty_map);

        // SECURITY: Remove tenant column from user input - tenant is ALWAYS set from Biscuit token
        // This prevents any attempt by users to override tenant isolation
        let mut final_args = obj.clone();
        if let Some(tc) = &tenant_column {
            final_args.remove(tc);
        }

        // Collect foreign key columns that need validation: (column_name, fk_constraint, referenced_table)
        let mut fk_columns: Vec<(String, cori_core::config::ForeignKeyConstraint, String)> = Vec::new();

        // Validate required fields and apply default values from role's creatable column constraints
        if let Some(creatable) = self.role.get_creatable_columns(table)
            && let Some(constraints_map) = creatable.as_map() {
                for (col_name, constraints) in constraints_map {
                    // Skip tenant column - it's handled separately from the token
                    if let Some(tc) = &tenant_column
                        && col_name == tc {
                            continue;
                        }

                    // Collect foreign key constraints for later validation
                    if let Some(fk_constraint) = &constraints.foreign_key {
                        // Look up the referenced table from schema
                        if let Some(ref_table) = self.get_fk_referenced_table(table, col_name) {
                            fk_columns.push((col_name.clone(), fk_constraint.clone(), ref_table));
                        }
                        // If not found in schema, skip FK validation (will be caught by DB constraints)
                    }

                    let has_value = final_args.contains_key(col_name);

                    // Check required constraint
                    if constraints.required && !has_value {
                        return ExecutionResult::error(format!(
                            "Missing required field: '{}'",
                            col_name
                        ));
                    }

                    // If column not provided and has a default, apply it
                    if !has_value
                        && let Some(default_val) = &constraints.default {
                            final_args.insert(col_name.clone(), default_val.clone());
                        }
                }
            }

        // Validate foreign keys with tenant isolation
        let pool = match &self.pool {
            Some(p) => p,
            None => {
                return ExecutionResult::error("Database connection not configured");
            }
        };

        // Track verification keys to remove after FK validation
        let mut verify_keys_to_remove: Vec<String> = Vec::new();

        for (fk_col, fk_constraint, ref_table) in &fk_columns {
            let fk_value = match final_args.get(fk_col) {
                Some(v) => v.clone(),
                None => continue, // FK not provided, will be caught by required check or is optional
            };

            // Get tenant column for the referenced table
            let ref_tenant_col = self.tenant_column_for_table(ref_table);

            // Determine the primary key column of the referenced table
            let ref_pk = self.get_primary_key_columns(ref_table);
            let pk_col = ref_pk.first().cloned().unwrap_or_else(|| {
                // Fallback: assume <table>_id pattern (customers -> customer_id)
                format!(
                    "{}_id",
                    ref_table.trim_end_matches('s')
                )
            });

            // Build validation query
            // SELECT 1 FROM <table> WHERE <pk> = <fk_value> AND <verify_cols match> AND <tenant> = <tenant_id>
            let mut conditions = vec![format!(
                "{} = {}",
                pk_col,
                self.value_to_sql_literal(&fk_value)
            )];

            // Add tenant condition for the referenced table
            if let Some(tc) = &ref_tenant_col {
                let tenant_literal = sql_string_literal(&context.tenant_id);
                conditions.push(format!("{} = {}", tc, tenant_literal));
            }

            // Add verify_with conditions
            for verify_col in &fk_constraint.verify_with {
                // The agent provides verification values with a prefix, e.g., "customer_email" for verifying customer
                let verify_key = format!(
                    "{}_{}",
                    ref_table.trim_end_matches('s'), // customers -> customer
                    verify_col
                );
                if let Some(verify_value) = final_args.get(&verify_key) {
                    conditions.push(format!(
                        "{} = {}",
                        verify_col,
                        self.value_to_sql_literal(verify_value)
                    ));
                    // Track for removal after validation
                    verify_keys_to_remove.push(verify_key);
                } else if let Some(verify_value) = obj.get(&verify_key) {
                    // Also check original args in case it was removed
                    conditions.push(format!(
                        "{} = {}",
                        verify_col,
                        self.value_to_sql_literal(verify_value)
                    ));
                }
            }

            let validation_query = format!(
                "SELECT 1 FROM {} WHERE {}",
                ref_table,
                conditions.join(" AND ")
            );

            tracing::debug!("Validating FK: {}", validation_query);

            match sqlx::query(&validation_query).fetch_optional(pool).await {
                Ok(Some(_)) => {
                    // FK is valid and belongs to the same tenant
                }
                Ok(None) => {
                    let verify_hint = if !fk_constraint.verify_with.is_empty() {
                        format!(
                            " with {} matching",
                            fk_constraint.verify_with.join(", ")
                        )
                    } else {
                        String::new()
                    };
                    return ExecutionResult::error(format!(
                        "Referenced {} not found{} (or belongs to a different tenant)",
                        ref_table.trim_end_matches('s'),
                        verify_hint
                    ));
                }
                Err(e) => {
                    return ExecutionResult::error(format!(
                        "Failed to validate foreign key: {}",
                        e
                    ));
                }
            }
        }

        // Remove verification columns from final_args (they're not part of the target table)
        for key in verify_keys_to_remove {
            final_args.remove(&key);
        }

        // Start with tenant column (if table is tenant-scoped)
        // SECURITY: Tenant value ALWAYS comes from the Biscuit token, never from user input
        let mut columns: Vec<String> = Vec::new();
        let mut value_strs: Vec<String> = Vec::new();
        if let Some(tc) = &tenant_column {
            columns.push(tc.clone());
            let tenant_value = sql_string_literal(&context.tenant_id);
            value_strs.push(tenant_value);
        }

        for (key, value) in &final_args {
            // Validate column name (alphanumeric and underscore only)
            if !key.chars().all(|c| c.is_alphanumeric() || c == '_') {
                continue;
            }
            columns.push(key.clone());
            // Convert value to SQL literal
            if let Some(s) = value.as_str() {
                value_strs.push(format!("'{}'", s.replace("'", "''")));
            } else if let Some(n) = value.as_i64() {
                value_strs.push(n.to_string());
            } else if let Some(f) = value.as_f64() {
                value_strs.push(f.to_string());
            } else if let Some(b) = value.as_bool() {
                value_strs.push(b.to_string());
            } else if value.is_null() {
                value_strs.push("NULL".to_string());
            } else {
                // For complex types, convert to JSON string
                value_strs.push(format!("'{}'", value.to_string().replace("'", "''")));
            }
        }

        let query = format!(
            "INSERT INTO {} ({}) VALUES ({}) RETURNING *",
            table,
            columns.join(", "),
            value_strs.join(", ")
        );

        tracing::debug!("Executing CREATE query: {}", query);

        match sqlx::query(&query).fetch_one(pool).await {
            Ok(row) => {
                let after_state = row_to_json(&row, &[]);
                // For create, before_state is None (record didn't exist)
                ExecutionResult::mutation_success(
                    json!({
                        "data": after_state.clone(),
                        "message": "Record created successfully"
                    }),
                    &query,
                    None, // No before state for CREATE
                    Some(after_state),
                )
            }
            Err(e) => ExecutionResult::error(format!("Database error: {}", e)),
        }
    }

    /// Execute an UPDATE operation.
    async fn execute_update(
        &self,
        table: &str,
        arguments: &Value,
        options: &CallToolOptions,
        context: &ExecutionContext,
    ) -> ExecutionResult {
        if let Err(e) = self.validate_tenant_for_table(table, &context.tenant_id) {
            return ExecutionResult::error(e);
        }

        // Get primary key columns from schema (supports composite keys)
        let pk_columns = self.get_primary_key_columns(table);

        // Cannot execute UPDATE without a primary key
        if pk_columns.is_empty() {
            return ExecutionResult::error(format!(
                "Cannot update record in table '{}': no primary key defined in schema",
                table
            ));
        }

        let tenant_column = self.tenant_column_for_table(table);

        // Extract all PK values from arguments
        let mut pk_values: Vec<(&str, i64)> = Vec::new();
        for pk_col in &pk_columns {
            match arguments.get(pk_col) {
                Some(v) => {
                    let val = v.as_i64().unwrap_or(0);
                    pk_values.push((pk_col, val));
                }
                None => {
                    return ExecutionResult::error(format!(
                        "Missing required primary key field: {}",
                        pk_col
                    ));
                }
            }
        }

        if options.dry_run {
            return ExecutionResult::dry_run(DryRunResult {
                dry_run: true,
                would_affect: json!({
                    table: { "update": 1 }
                }),
                preview: Some(json!({
                    "operation": "UPDATE",
                    "table": table,
                    "pk_columns": pk_columns,
                    "changes": arguments,
                    "tenant_id": context.tenant_id
                })),
            });
        }

        // Execute actual update
        let pool = match &self.pool {
            Some(p) => p,
            None => {
                return ExecutionResult::error("Database connection not configured");
            }
        };

        // Get updatable columns for this table from role definition
        let updatable_columns = self.role.tables.get(table).map(|p| &p.updatable);

        // Build UPDATE statement from arguments (excluding primary key columns)
        let empty_map = serde_json::Map::new();
        let obj = arguments.as_object().unwrap_or(&empty_map);
        let mut set_clauses = Vec::new();
        let mut rejected_columns = Vec::new();

        for (key, value) in obj {
            // Skip primary key columns
            if pk_columns.contains(key) {
                continue;
            }
            // Validate column name (alphanumeric and underscore only)
            if !key.chars().all(|c| c.is_alphanumeric() || c == '_') {
                continue;
            }

            // Check if column is updatable according to role permissions
            let is_updatable = match updatable_columns {
                Some(cols) => cols.contains(key),
                None => false, // If no table permissions, default to not updatable
            };

            if !is_updatable {
                rejected_columns.push(key.clone());
                continue; // Skip non-updatable columns
            }

            // Check only_when constraint from role definition for restrict_to pattern
            if let Some(cols) = updatable_columns
                && let Some(constraints) = cols.get_constraints(key) {
                    // Check if there's a simple new.<col>: [values] restriction
                    if let Some(only_when) = &constraints.only_when
                        && let Some(allowed_values) = only_when.get_new_value_restriction(key)
                            && !allowed_values.contains(value) {
                                return ExecutionResult::error(format!(
                                    "Value '{}' for column '{}' not in allowed values: {:?}",
                                    value, key, allowed_values
                                ));
                            }
                }

            // Build SET clause with inline values
            if let Some(s) = value.as_str() {
                set_clauses.push(format!("{} = '{}'", key, s.replace("'", "''")));
            } else if let Some(n) = value.as_i64() {
                set_clauses.push(format!("{} = {}", key, n));
            } else if let Some(b) = value.as_bool() {
                set_clauses.push(format!("{} = {}", key, b));
            } else if value.is_null() {
                set_clauses.push(format!("{} = NULL", key));
            }
        }

        // Log rejected columns for audit/debugging
        if !rejected_columns.is_empty() {
            tracing::warn!(
                "UPDATE on {} rejected non-updatable columns: {:?}",
                table,
                rejected_columns
            );
        }

        if set_clauses.is_empty() {
            return ExecutionResult::error(format!(
                "No updatable fields provided. Rejected columns: {:?}",
                rejected_columns
            ));
        }

        // Build primary key conditions
        let pk_conditions: Vec<String> = pk_values
            .iter()
            .map(|(col, val)| format!("{} = {}", col, val))
            .collect();

        // Build optional tenant condition - embed directly since it comes from trusted token
        let tenant_condition = tenant_column.as_ref().map(|tc| {
            format!("{} = {}", tc, sql_string_literal(&context.tenant_id))
        });

        // Capture before state for audit (using first PK value)
        let before_state = if let Some((_, pk_val)) = pk_values.first() {
            self.fetch_row_snapshot(table, *pk_val, &context.tenant_id)
                .await
        } else {
            None
        };

        let query = if let Some(tc) = tenant_condition {
            format!(
                "UPDATE {} SET {} WHERE {} AND {} RETURNING *",
                table,
                set_clauses.join(", "),
                pk_conditions.join(" AND "),
                tc
            )
        } else {
            format!(
                "UPDATE {} SET {} WHERE {} RETURNING *",
                table,
                set_clauses.join(", "),
                pk_conditions.join(" AND ")
            )
        };

        tracing::debug!("Executing UPDATE query: {}", query);

        match sqlx::query(&query).fetch_optional(pool).await {
            Ok(Some(row)) => {
                let after_state = row_to_json(&row, &[]);
                ExecutionResult::mutation_success(
                    json!({
                        "data": after_state.clone(),
                        "message": "Record updated successfully"
                    }),
                    &query,
                    before_state,
                    Some(after_state),
                )
            }
            Ok(None) => {
                // Provide more specific error message
                let pk_description = pk_values
                    .iter()
                    .map(|(col, val)| format!("{} = {}", col, val))
                    .collect::<Vec<_>>()
                    .join(" AND ");
                if tenant_column.is_some() {
                    ExecutionResult::error(format!(
                        "Record not found: no {} with {} exists for tenant '{}'. \
                        The record may not exist or may belong to a different tenant.",
                        table, pk_description, context.tenant_id
                    ))
                } else {
                    ExecutionResult::error(format!(
                        "Record not found: no {} with {} exists.",
                        table, pk_description
                    ))
                }
            }
            Err(e) => ExecutionResult::error(format!("Database error: {}", e)),
        }
    }

    /// Execute a DELETE operation.
    async fn execute_delete(
        &self,
        table: &str,
        arguments: &Value,
        options: &CallToolOptions,
        context: &ExecutionContext,
    ) -> ExecutionResult {
        if let Err(e) = self.validate_tenant_for_table(table, &context.tenant_id) {
            return ExecutionResult::error(e);
        }

        // Get primary key columns from schema (supports composite keys)
        let pk_columns = self.get_primary_key_columns(table);

        // Cannot execute DELETE without a primary key
        if pk_columns.is_empty() {
            return ExecutionResult::error(format!(
                "Cannot delete record from table '{}': no primary key defined in schema",
                table
            ));
        }

        let tenant_column = self.tenant_column_for_table(table);

        // Extract all PK values from arguments
        let mut pk_values: Vec<(&str, i64)> = Vec::new();
        for pk_col in &pk_columns {
            match arguments.get(pk_col) {
                Some(v) => {
                    let val = v.as_i64().unwrap_or(0);
                    pk_values.push((pk_col, val));
                }
                None => {
                    return ExecutionResult::error(format!(
                        "Missing required primary key field: {}",
                        pk_col
                    ));
                }
            }
        }

        if options.dry_run {
            return ExecutionResult::dry_run(DryRunResult {
                dry_run: true,
                would_affect: json!({
                    table: { "delete": 1 }
                }),
                preview: Some(json!({
                    "operation": "DELETE",
                    "table": table,
                    "pk_columns": pk_columns,
                    "tenant_id": context.tenant_id
                })),
            });
        }

        // Execute actual delete
        let pool = match &self.pool {
            Some(p) => p,
            None => {
                return ExecutionResult::error("Database connection not configured");
            }
        };

        // Build primary key conditions
        let pk_conditions: Vec<String> = pk_values
            .iter()
            .map(|(col, val)| format!("{} = {}", col, val))
            .collect();

        // Build optional tenant condition - embed directly since it comes from trusted token
        let tenant_condition = tenant_column.as_ref().map(|tc| {
            format!("{} = {}", tc, sql_string_literal(&context.tenant_id))
        });

        // Capture before state for audit (using first PK value)
        let before_state = if let Some((_, pk_val)) = pk_values.first() {
            self.fetch_row_snapshot(table, *pk_val, &context.tenant_id)
                .await
        } else {
            None
        };

        // Build RETURNING clause with all PK columns
        let returning_cols = pk_columns.join(", ");

        let query = if let Some(tc) = tenant_condition {
            format!(
                "DELETE FROM {} WHERE {} AND {} RETURNING {}",
                table,
                pk_conditions.join(" AND "),
                tc,
                returning_cols
            )
        } else {
            format!(
                "DELETE FROM {} WHERE {} RETURNING {}",
                table,
                pk_conditions.join(" AND "),
                returning_cols
            )
        };

        tracing::debug!("Executing DELETE query: {}", query);

        match sqlx::query(&query).fetch_optional(pool).await {
            Ok(Some(row)) => {
                // Build response with all PK values
                let deleted_keys = row_to_json(&row, &pk_columns);
                // For delete, after_state is null (record is gone)
                ExecutionResult::mutation_success(
                    json!({
                        "message": "Record deleted successfully",
                        "deleted": deleted_keys
                    }),
                    &query,
                    before_state,
                    None, // Record no longer exists
                )
            }
            Ok(None) => {
                let pk_description = pk_values
                    .iter()
                    .map(|(col, val)| format!("{} = {}", col, val))
                    .collect::<Vec<_>>()
                    .join(" AND ");
                if tenant_column.is_some() {
                    ExecutionResult::error(format!(
                        "Record not found: no {} with {} exists for tenant '{}'. \
                        The record may not exist or may belong to a different tenant.",
                        table, pk_description, context.tenant_id
                    ))
                } else {
                    ExecutionResult::error(format!(
                        "Record not found: no {} with {} exists.",
                        table, pk_description
                    ))
                }
            }
            Err(e) => ExecutionResult::error(format!("Database error: {}", e)),
        }
    }

    /// Execute a custom action (not currently supported).
    async fn execute_custom(
        &self,
        name: &str,
        _arguments: &Value,
        _options: &CallToolOptions,
        _context: &ExecutionContext,
    ) -> ExecutionResult {
        // Custom actions are not supported in the current version
        ExecutionResult::error(format!(
            "Custom action '{}' is not supported. Only standard CRUD operations (get, list, create, update, delete) are available.",
            name
        ))
    }
}

/// Parsed tool operation.
#[derive(Debug)]
enum ToolOperation {
    Get { table: String },
    List { table: String },
    Create { table: String },
    Update { table: String },
    Delete { table: String },
    Custom { name: String },
}

/// Convert PascalCase to snake_case.
fn snake_case(s: &str) -> String {
    let mut result = String::new();
    for (i, c) in s.chars().enumerate() {
        if c.is_uppercase() {
            if i > 0 {
                result.push('_');
            }
            result.push(c.to_lowercase().next().unwrap());
        } else {
            result.push(c);
        }
    }
    result
}

/// Simple pluralization.
fn pluralize(s: &str) -> String {
    if s.ends_with('s') || s.ends_with('x') || s.ends_with("ch") || s.ends_with("sh") {
        format!("{}es", s)
    } else if s.ends_with('y') && !s.ends_with("ey") && !s.ends_with("ay") && !s.ends_with("oy") {
        format!("{}ies", &s[..s.len() - 1])
    } else {
        format!("{}s", s)
    }
}

/// Convert a sqlx row to JSON, using provided columns or all columns if empty.
fn row_to_json(row: &sqlx::postgres::PgRow, columns: &[String]) -> Value {
    use bigdecimal::BigDecimal;
    use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};
    use sqlx::Column;

    let mut obj = serde_json::Map::new();

    for col in row.columns() {
        let name = col.name();

        // If columns list is provided and non-empty, filter
        if !columns.is_empty() && !columns.iter().any(|c| c == name) {
            continue;
        }

        // Try to extract the value as different types
        // Order matters: try more specific types first
        let value: Value =
            // Integer types
            if let Ok(v) = row.try_get::<i64, _>(name) {
                json!(v)
            } else if let Ok(v) = row.try_get::<i32, _>(name) {
                json!(v)
            } else if let Ok(v) = row.try_get::<i16, _>(name) {
                json!(v)
            }
            // Floating point
            else if let Ok(v) = row.try_get::<f64, _>(name) {
                json!(v)
            } else if let Ok(v) = row.try_get::<f32, _>(name) {
                json!(v)
            }
            // BigDecimal for DECIMAL/NUMERIC columns
            else if let Ok(v) = row.try_get::<BigDecimal, _>(name) {
                // Convert to f64 for JSON, preserving precision
                use bigdecimal::ToPrimitive;
                json!(v.to_f64().unwrap_or(0.0))
            }
            // Optional BigDecimal
            else if let Ok(v) = row.try_get::<Option<BigDecimal>, _>(name) {
                match v {
                    Some(d) => {
                        use bigdecimal::ToPrimitive;
                        json!(d.to_f64().unwrap_or(0.0))
                    }
                    None => Value::Null,
                }
            }
            // Boolean
            else if let Ok(v) = row.try_get::<bool, _>(name) {
                json!(v)
            }
            // Timestamp with timezone
            else if let Ok(v) = row.try_get::<DateTime<Utc>, _>(name) {
                json!(v.to_rfc3339())
            }
            // Optional timestamp with timezone
            else if let Ok(v) = row.try_get::<Option<DateTime<Utc>>, _>(name) {
                match v {
                    Some(dt) => json!(dt.to_rfc3339()),
                    None => Value::Null,
                }
            }
            // Timestamp without timezone
            else if let Ok(v) = row.try_get::<NaiveDateTime, _>(name) {
                json!(v.format("%Y-%m-%dT%H:%M:%S").to_string())
            }
            // Optional timestamp without timezone
            else if let Ok(v) = row.try_get::<Option<NaiveDateTime>, _>(name) {
                match v {
                    Some(dt) => json!(dt.format("%Y-%m-%dT%H:%M:%S").to_string()),
                    None => Value::Null,
                }
            }
            // Date
            else if let Ok(v) = row.try_get::<NaiveDate, _>(name) {
                json!(v.format("%Y-%m-%d").to_string())
            }
            // Optional date
            else if let Ok(v) = row.try_get::<Option<NaiveDate>, _>(name) {
                match v {
                    Some(d) => json!(d.format("%Y-%m-%d").to_string()),
                    None => Value::Null,
                }
            }
            // Time
            else if let Ok(v) = row.try_get::<NaiveTime, _>(name) {
                json!(v.format("%H:%M:%S").to_string())
            }
            // Optional time
            else if let Ok(v) = row.try_get::<Option<NaiveTime>, _>(name) {
                match v {
                    Some(t) => json!(t.format("%H:%M:%S").to_string()),
                    None => Value::Null,
                }
            }
            // UUID
            else if let Ok(v) = row.try_get::<uuid::Uuid, _>(name) {
                json!(v.to_string())
            }
            // Optional UUID
            else if let Ok(v) = row.try_get::<Option<uuid::Uuid>, _>(name) {
                match v {
                    Some(u) => json!(u.to_string()),
                    None => Value::Null,
                }
            }
            // String
            else if let Ok(v) = row.try_get::<String, _>(name) {
                json!(v)
            }
            // JSON/JSONB
            else if let Ok(v) = row.try_get::<serde_json::Value, _>(name) {
                v
            }
            // Optional string (fallback)
            else if let Ok(v) = row.try_get::<Option<String>, _>(name) {
                match v {
                    Some(s) => json!(s),
                    None => Value::Null,
                }
            }
            // String array
            else if let Ok(v) = row.try_get::<Vec<String>, _>(name) {
                json!(v)
            }
            // Optional string array
            else if let Ok(v) = row.try_get::<Option<Vec<String>>, _>(name) {
                match v {
                    Some(arr) => json!(arr),
                    None => Value::Null,
                }
            }
            // Final fallback
            else {
                Value::Null
            };

        obj.insert(name.to_string(), value);
    }

    Value::Object(obj)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_snake_case() {
        assert_eq!(snake_case("User"), "user");
        assert_eq!(snake_case("OrderItem"), "order_item");
        assert_eq!(snake_case("APIKey"), "a_p_i_key");
    }

    #[test]
    fn test_parse_tool_operation() {
        let role = RoleDefinition {
            name: "test".to_string(),
            description: None,
            approvals: None,
            tables: std::collections::HashMap::new(),
        };
        let approval_manager = Arc::new(ApprovalManager::default());
        let executor = ToolExecutor::new(role, approval_manager);

        matches!(
            executor.parse_tool_operation("getUser"),
            ToolOperation::Get { table } if table == "user"
        );
        matches!(
            executor.parse_tool_operation("listUsers"),
            ToolOperation::List { table } if table == "user"
        );
        matches!(
            executor.parse_tool_operation("createTicket"),
            ToolOperation::Create { table } if table == "ticket"
        );
    }
}

/// A tenant id as a SQL string literal. Always quoted: an untyped literal coerces to an integer tenant column, and a
/// numeric-looking id (a user sub such as "42") must still compare against a text column.
fn sql_string_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}
