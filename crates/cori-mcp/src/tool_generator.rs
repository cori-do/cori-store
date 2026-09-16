//! Role-driven dynamic tool generation.
//!
//! This module generates MCP tools dynamically based on role permissions
//! and database schema. Tools are generated at connection time based on
//! the connecting token's role claims.
//!
//! ## Tool Generation Rules
//!
//! | Tool Pattern | Generated When | Description |
//! |--------------|----------------|-------------|
//! | `get<Entity>(id)` | Table has readable columns | Fetch single record |
//! | `list<Entity>(filters)` | Table has readable columns | Query multiple records |
//! | `create<Entity>(data)` | Role has creatable columns | Create new record |
//! | `update<Entity>(id, data)` | Role has updatable columns | Modify record |
//! | `delete<Entity>(id)` | Role has DELETE permission | Remove record |

use crate::protocol::{ToolAnnotations, ToolDefinition};
use crate::schema::{DatabaseSchema, TableSchema};
use cori_core::{
    ColumnList, CreatableColumnConstraints, CreatableColumns, ReadableConfig, RoleDefinition,
    UpdatableColumnConstraints, UpdatableColumns,
};
use serde_json::{Value, json};

/// Generator for creating MCP tools from role permissions and schema.
pub struct ToolGenerator {
    /// The role definition.
    role: RoleDefinition,
    /// The database schema.
    schema: DatabaseSchema,
}

impl ToolGenerator {
    /// Create a new tool generator.
    pub fn new(role: RoleDefinition, schema: DatabaseSchema) -> Self {
        Self { role, schema }
    }

    /// Generate all tools for this role.
    ///
    /// Panics if a table in the role is not found in the schema.
    pub fn generate_all(&self) -> Vec<ToolDefinition> {
        let mut tools = Vec::new();

        // Generate CRUD tools for each accessible table
        for (table_name, perms) in &self.role.tables {
            let table_schema = self.schema.get_table(table_name)
                .unwrap_or_else(|| panic!("Table '{}' in role '{}' not found in schema. Run 'cori db sync' to update schema.", table_name, self.role.name));

            tracing::debug!(
                table = %table_name,
                role = %self.role.name,
                can_update = %self.role.can_update(table_name),
                updatable_cols = ?perms.updatable,
                has_pk = !table_schema.primary_key.is_empty(),
                pk = ?table_schema.primary_key,
                "Generating tools for table"
            );

            let table_tools = self.generate_table_tools(table_name, table_schema);
            tracing::debug!(
                table = %table_name,
                tools = ?table_tools.iter().map(|t| &t.name).collect::<Vec<_>>(),
                "Generated tools"
            );
            tools.extend(table_tools);
        }

        tools
    }

    /// Generate CRUD tools for a single table.
    fn generate_table_tools(
        &self,
        table_name: &str,
        table_schema: &TableSchema,
    ) -> Vec<ToolDefinition> {
        let mut tools = Vec::new();
        // Singularize the table name before converting to PascalCase for entity name
        let singular_name = singularize(table_name);
        let entity_name = pascal_case(&singular_name);

        // Check if table has a primary key (needed for get, update, delete operations)
        let has_primary_key = !table_schema.primary_key.is_empty();

        tracing::debug!(
            table = %table_name,
            has_pk = has_primary_key,
            can_update = self.role.can_update(table_name),
            pk_columns = ?table_schema.primary_key,
            "Generating tools for table"
        );

        // get{Entity} - if readable AND table has primary key
        if self.role.can_read(table_name) {
            // get requires primary key to identify a single record
            if has_primary_key {
                tools.push(self.generate_get_tool(table_name, &entity_name, table_schema));
            }
            // list always works (no PK needed)
            tools.push(self.generate_list_tool(table_name, &entity_name, table_schema));
        }

        // create{Entity} - if can create (no PK needed for insert)
        if self.role.can_create(table_name) {
            tools.push(self.generate_create_tool(table_name, &entity_name, table_schema));
        }

        // update{Entity} - if can update AND table has primary key
        if self.role.can_update(table_name) && has_primary_key {
            tracing::debug!(table = %table_name, "Generating update tool");
            tools.push(self.generate_update_tool(table_name, &entity_name, table_schema));
        }

        // delete{Entity} - if can delete AND table has primary key
        if self.role.can_delete(table_name) && has_primary_key {
            tools.push(self.generate_delete_tool(table_name, &entity_name, table_schema));
        }

        tools
    }

    /// Generate a get{Entity} tool.
    fn generate_get_tool(
        &self,
        table_name: &str,
        entity_name: &str,
        table_schema: &TableSchema,
    ) -> ToolDefinition {
        let pk_names = table_schema.get_primary_key_names();
        let pk_columns = table_schema.get_primary_key_columns();

        // Build properties for all PK columns
        let mut properties = serde_json::Map::new();
        for col in &pk_columns {
            properties.insert(
                col.name.clone(),
                json!({
                    "type": col.json_schema_type(),
                    "description": format!("{} primary key column ({})", entity_name, col.name)
                }),
            );
        }

        let pk_desc = if pk_names.len() == 1 {
            pk_names[0].to_string()
        } else {
            pk_names.join(", ")
        };

        ToolDefinition {
            name: format!("get{}", entity_name),
            description: Some(format!(
                "Retrieve one {} by primary key ({})",
                singularize(table_name), pk_desc
            )),
            input_schema: json!({
                "type": "object",
                "properties": properties,
                "required": pk_names
            }),
            annotations: Some(ToolAnnotations {
                requires_approval: Some(false),
                dry_run_supported: Some(false),
                read_only: Some(true),
                ..Default::default()
            }),
        }
    }

    /// Generate a list{Entities} tool.
    fn generate_list_tool(
        &self,
        table_name: &str,
        entity_name: &str,
        table_schema: &TableSchema,
    ) -> ToolDefinition {
        let max_per_page = self.role.get_max_per_page(table_name).unwrap_or(1000);
        let mut properties = json!({
            "limit": {
                "type": "integer",
                "default": 50,
                "maximum": max_per_page,
                "description": "Maximum number of results"
            },
            "offset": {
                "type": "integer",
                "default": 0,
                "description": "Offset for pagination"
            }
        });

        // Add filter properties for readable columns
        let filter_props = self.generate_filter_properties(table_name, table_schema);
        if let Value::Object(ref mut props) = properties
            && let Value::Object(filters) = filter_props {
                props.extend(filters);
            }

        ToolDefinition {
            name: format!("list{}", pluralize(entity_name)),
            description: Some(format!(
                "List {} with optional filters",
                table_name
            )),
            input_schema: json!({
                "type": "object",
                "properties": properties
            }),
            annotations: Some(ToolAnnotations {
                requires_approval: Some(false),
                dry_run_supported: Some(false),
                read_only: Some(true),
                ..Default::default()
            }),
        }
    }

    /// Generate a create{Entity} tool.
    fn generate_create_tool(
        &self,
        table_name: &str,
        entity_name: &str,
        table_schema: &TableSchema,
    ) -> ToolDefinition {
        let (properties, required) = self.generate_input_schema(table_name, table_schema, true);
        let requires_approval = self.role.table_requires_approval(table_name);
        let approval_fields = self.role.get_approval_columns(table_name);

        ToolDefinition {
            name: format!("create{}", entity_name),
            description: Some(format!("Create a new {}", singularize(table_name))),
            input_schema: json!({
                "type": "object",
                "properties": properties,
                "required": required
            }),
            annotations: Some(ToolAnnotations {
                requires_approval: Some(requires_approval),
                dry_run_supported: Some(true),
                read_only: Some(false),
                approval_fields: if approval_fields.is_empty() {
                    None
                } else {
                    Some(approval_fields.iter().map(|s| s.to_string()).collect())
                },
            }),
        }
    }

    /// Generate an update{Entity} tool.
    fn generate_update_tool(
        &self,
        table_name: &str,
        entity_name: &str,
        table_schema: &TableSchema,
    ) -> ToolDefinition {
        let (mut properties, _) = self.generate_input_schema(table_name, table_schema, false);
        let requires_approval = self.role.table_requires_approval(table_name);
        let approval_fields = self.role.get_approval_columns(table_name);
        let pk_names = table_schema.get_primary_key_names();
        let pk_columns = table_schema.get_primary_key_columns();

        // Add all primary key columns to properties
        if let Value::Object(ref mut props) = properties {
            for col in &pk_columns {
                props.insert(
                    col.name.clone(),
                    json!({
                        "type": col.json_schema_type(),
                        "description": format!("{} primary key column ({})", entity_name, col.name)
                    }),
                );
            }
        }

        let pk_desc = if pk_names.len() == 1 {
            pk_names[0].to_string()
        } else {
            pk_names.join(", ")
        };

        ToolDefinition {
            name: format!("update{}", entity_name),
            description: Some(format!(
                "Update an existing {} by primary key ({})",
                singularize(table_name), pk_desc
            )),
            input_schema: json!({
                "type": "object",
                "properties": properties,
                "required": pk_names
            }),
            annotations: Some(ToolAnnotations {
                requires_approval: Some(requires_approval),
                dry_run_supported: Some(true),
                read_only: Some(false),
                approval_fields: if approval_fields.is_empty() {
                    None
                } else {
                    Some(approval_fields.iter().map(|s| s.to_string()).collect())
                },
            }),
        }
    }

    /// Generate a delete{Entity} tool.
    fn generate_delete_tool(
        &self,
        table_name: &str,
        entity_name: &str,
        table_schema: &TableSchema,
    ) -> ToolDefinition {
        let pk_names = table_schema.get_primary_key_names();
        let pk_columns = table_schema.get_primary_key_columns();

        // Build properties for all PK columns
        let mut properties = serde_json::Map::new();
        for col in &pk_columns {
            properties.insert(
                col.name.clone(),
                json!({
                    "type": col.json_schema_type(),
                    "description": format!("{} primary key column ({})", entity_name, col.name)
                }),
            );
        }

        // Add verify_with columns for deletable constraints (optional verification)
        let required_fields: Vec<String> = pk_names.iter().map(|s| s.to_string()).collect();
        if let Some(perms) = self.role.tables.get(table_name) {
            if let cori_core::DeletablePermission::WithConstraints(constraints) = &perms.deletable {
                for verify_col in &constraints.verify_with {
                    // Get the column schema from the same table for type info
                    let col_type = table_schema
                        .get_column(verify_col)
                        .map(|c| c.json_schema_type())
                        .unwrap_or("string");

                    properties.insert(
                        verify_col.clone(),
                        json!({
                            "type": col_type,
                            "description": format!("Optional verification column '{}' to confirm the record being deleted", verify_col)
                        }),
                    );
                    // verify_with columns are optional for delete (not added to required_fields)
                }
            }
        }

        let pk_desc = if required_fields.len() == 1 {
            required_fields[0].to_string()
        } else {
            required_fields.join(", ")
        };

        ToolDefinition {
            name: format!("delete{}", entity_name),
            description: Some(format!(
                "Delete one {} by primary key ({})",
                singularize(table_name), pk_desc
            )),
            input_schema: json!({
                "type": "object",
                "properties": properties,
                "required": pk_names
            }),
            annotations: Some(ToolAnnotations {
                requires_approval: Some(true),
                dry_run_supported: Some(true),
                read_only: Some(false),
                ..Default::default()
            }),
        }
    }

    /// Generate input schema properties for creatable/updatable columns.
    fn generate_input_schema(
        &self,
        table_name: &str,
        table_schema: &TableSchema,
        for_create: bool,
    ) -> (Value, Vec<String>) {
        let mut properties = serde_json::Map::new();
        let mut required = Vec::new();

        if let Some(perms) = self.role.tables.get(table_name) {
            if for_create {
                // For create operations, use creatable columns
                self.add_creatable_columns_to_schema(
                    &perms.creatable,
                    table_schema,
                    &mut properties,
                    &mut required,
                );
            } else {
                // For update operations, use updatable columns
                self.add_updatable_columns_to_schema(
                    &perms.updatable,
                    table_schema,
                    &mut properties,
                );
            }
        }

        (Value::Object(properties), required)
    }

    /// Add creatable columns to the JSON schema properties.
    fn add_creatable_columns_to_schema(
        &self,
        creatable: &CreatableColumns,
        table_schema: &TableSchema,
        properties: &mut serde_json::Map<String, Value>,
        required: &mut Vec<String>,
    ) {
        match creatable {
            CreatableColumns::All(_) => {
                // All columns are creatable
                for col in &table_schema.columns {
                    let prop = self.column_to_basic_json_schema(&col.name, table_schema);
                    properties.insert(col.name.clone(), prop);
                    if !col.nullable && col.default.is_none() {
                        required.push(col.name.clone());
                    }
                }
            }
            CreatableColumns::Map(map) => {
                for (col_name, constraints) in map {
                    if let Some(col_schema) = table_schema.get_column(col_name) {
                        let prop =
                            self.creatable_constraints_to_json_schema(constraints, col_schema);
                        properties.insert(col_name.clone(), prop);
                        // Required if constraint says so, or if non-nullable without default
                        if constraints.required
                            || (!col_schema.nullable && col_schema.default.is_none())
                        {
                            required.push(col_name.clone());
                        }
                    } else {
                        // Column not in schema, use basic string type
                        properties.insert(
                            col_name.clone(),
                            self.creatable_constraints_to_basic_json_schema(constraints),
                        );
                        if constraints.required {
                            required.push(col_name.clone());
                        }
                    }

                    // Add verify_with columns for foreign key constraints
                    if let Some(fk) = &constraints.foreign_key {
                        // Look up the referenced table from schema
                        if let Some(ref_table) = self.get_fk_referenced_table(&table_schema.name, col_name) {
                            self.add_verify_with_columns(
                                col_name,
                                &ref_table,
                                &fk.verify_with,
                                properties,
                                required,
                            );
                        }
                    }
                }
            }
        }
    }

    /// Add updatable columns to the JSON schema properties.
    fn add_updatable_columns_to_schema(
        &self,
        updatable: &UpdatableColumns,
        table_schema: &TableSchema,
        properties: &mut serde_json::Map<String, Value>,
    ) {
        match updatable {
            UpdatableColumns::All(_) => {
                // All columns are updatable
                for col in &table_schema.columns {
                    let prop = self.column_to_basic_json_schema(&col.name, table_schema);
                    properties.insert(col.name.clone(), prop);
                }
            }
            UpdatableColumns::Map(map) => {
                for (col_name, constraints) in map {
                    if let Some(col_schema) = table_schema.get_column(col_name) {
                        let prop = self.updatable_constraints_to_json_schema(
                            constraints,
                            col_schema,
                            col_name,
                        );
                        properties.insert(col_name.clone(), prop);
                    } else {
                        // Column not in schema, use basic string type
                        properties.insert(
                            col_name.clone(),
                            self.updatable_constraints_to_basic_json_schema(constraints, col_name),
                        );
                    }

                    // Add verify_with columns for foreign key constraints
                    if let Some(fk) = &constraints.foreign_key {
                        // Look up the referenced table from schema
                        if let Some(ref_table) = self.get_fk_referenced_table(&table_schema.name, col_name) {
                            // For updates, verify_with columns are not required (only FK column update is optional)
                            let mut dummy_required = Vec::new();
                            self.add_verify_with_columns(
                                col_name,
                                &ref_table,
                                &fk.verify_with,
                                properties,
                                &mut dummy_required,
                            );
                        }
                    }
                }
            }
        }
    }

    /// Generate filter properties for readable columns.
    fn generate_filter_properties(&self, table_name: &str, table_schema: &TableSchema) -> Value {
        let mut properties = serde_json::Map::new();

        if let Some(perms) = self.role.tables.get(table_name) {
            // Get list of readable columns from ReadableConfig
            let readable_cols: Vec<&str> = match &perms.readable {
                ReadableConfig::All(_) => table_schema
                    .columns
                    .iter()
                    .map(|c| c.name.as_str())
                    .collect(),
                ReadableConfig::List(cols) => cols.iter().map(|s: &String| s.as_str()).collect(),
                ReadableConfig::Config(cfg) => match &cfg.columns {
                    ColumnList::All(_) => table_schema
                        .columns
                        .iter()
                        .map(|c| c.name.as_str())
                        .collect(),
                    ColumnList::List(cols) => cols.iter().map(|s: &String| s.as_str()).collect(),
                },
            };

            for col_name in readable_cols {
                if let Some(col_schema) = table_schema.get_column(col_name) {
                    // Skip complex types for filtering
                    let col_type = col_schema.json_schema_type();
                    if col_type == "object" || col_type == "array" {
                        continue;
                    }

                    let mut prop = json!({
                        "type": col_type,
                        "description": format!("Filter by {}", col_name)
                    });

                    if let Some(format) = col_schema.json_schema_format() {
                        prop["format"] = json!(format);
                    }

                    properties.insert(col_name.to_string(), prop);
                }
            }
        }

        Value::Object(properties)
    }

    /// Convert a column to basic JSON schema.
    fn column_to_basic_json_schema(&self, col_name: &str, table_schema: &TableSchema) -> Value {
        if let Some(col) = table_schema.get_column(col_name) {
            let base_type = col.json_schema_type();
            let mut schema = json!({ "type": base_type });
            if let Some(format) = col.json_schema_format() {
                schema["format"] = json!(format);
            }
            if let Some(desc) = &col.description {
                schema["description"] = json!(desc);
            }
            schema
        } else {
            json!({ "type": "string" })
        }
    }

    /// Add verify_with columns to the JSON schema for foreign key verification.
    ///
    /// For a FK column like `customer_id` with `verify_with: [email]`, this adds
    /// a property `customer_email` (stripping `_id` suffix and prefixing verify column).
    fn add_verify_with_columns(
        &self,
        fk_col_name: &str,
        ref_table: &str,
        verify_with: &[String],
        properties: &mut serde_json::Map<String, Value>,
        required: &mut Vec<String>,
    ) {
        if verify_with.is_empty() {
            return;
        }

        // Get the prefix from the FK column name (e.g., "customer_id" -> "customer")
        let prefix = fk_col_name
            .strip_suffix("_id")
            .unwrap_or(fk_col_name);

        // Get the referenced table's schema for type info
        let ref_table_schema = self.schema.get_table(ref_table);

        for verify_col in verify_with {
            // Create property name like "customer_email" from FK "customer_id" and verify_with "email"
            let prop_name = format!("{}_{}", prefix, verify_col);

            // Get type from the referenced table's column schema
            let col_type = ref_table_schema
                .and_then(|t| t.get_column(verify_col))
                .map(|c| c.json_schema_type())
                .unwrap_or("string");

            let col_format = ref_table_schema
                .and_then(|t| t.get_column(verify_col))
                .and_then(|c| c.json_schema_format());

            let mut prop = json!({
                "type": col_type,
                "description": format!(
                    "Verification value for '{}' column from '{}' table (must match the referenced record)",
                    verify_col, ref_table
                )
            });

            if let Some(format) = col_format {
                prop["format"] = json!(format);
            }

            properties.insert(prop_name.clone(), prop);
            // verify_with columns are required when the FK column is provided
            required.push(prop_name);
        }
    }

    /// Get the referenced table for a foreign key column from schema.
    /// Returns None if no FK is found.
    fn get_fk_referenced_table(&self, table: &str, column: &str) -> Option<String> {
        let table_schema = self.schema.get_table(table)?;
        
        for fk in &table_schema.foreign_keys {
            for fk_col in &fk.columns {
                if fk_col.column == column {
                    return Some(fk_col.foreign_table.clone());
                }
            }
        }
        None
    }

    /// Convert creatable column constraints to JSON Schema.
    fn creatable_constraints_to_json_schema(
        &self,
        constraints: &CreatableColumnConstraints,
        col_schema: &crate::schema::ColumnSchema,
    ) -> Value {
        let base_type = col_schema.json_schema_type();
        let mut schema = json!({ "type": base_type });

        // Add restrict_to as enum
        if let Some(values) = &constraints.restrict_to {
            schema["enum"] = json!(values);
        }

        // Add default
        if let Some(default) = &constraints.default {
            schema["default"] = default.clone();
        }

        // Add format if applicable
        if let Some(format) = col_schema.json_schema_format() {
            schema["format"] = json!(format);
        }

        // Add description/guidance
        if let Some(guidance) = &constraints.guidance {
            schema["description"] = json!(guidance);
        } else if let Some(desc) = &col_schema.description {
            schema["description"] = json!(desc);
        }

        schema
    }

    /// Convert creatable constraints to basic JSON Schema (no column schema).
    fn creatable_constraints_to_basic_json_schema(
        &self,
        constraints: &CreatableColumnConstraints,
    ) -> Value {
        let mut schema = json!({ "type": "string" });

        if let Some(values) = &constraints.restrict_to {
            schema["enum"] = json!(values);
        }

        if let Some(default) = &constraints.default {
            schema["default"] = default.clone();
        }

        if let Some(guidance) = &constraints.guidance {
            schema["description"] = json!(guidance);
        }

        schema
    }

    /// Convert updatable column constraints to JSON Schema.
    fn updatable_constraints_to_json_schema(
        &self,
        constraints: &UpdatableColumnConstraints,
        col_schema: &crate::schema::ColumnSchema,
        col_name: &str,
    ) -> Value {
        let base_type = col_schema.json_schema_type();
        let mut schema = json!({ "type": base_type });

        // Extract enum values from only_when if it's a simple new.<col>: [values] pattern
        if let Some(only_when) = &constraints.only_when
            && let Some(values) = only_when.get_new_value_restriction(col_name) {
                schema["enum"] = json!(values);
            }

        // Add format if applicable
        if let Some(format) = col_schema.json_schema_format() {
            schema["format"] = json!(format);
        }

        // Add description/guidance
        if let Some(guidance) = &constraints.guidance {
            schema["description"] = json!(guidance);
        } else if let Some(desc) = &col_schema.description {
            schema["description"] = json!(desc);
        }

        schema
    }

    /// Convert updatable constraints to basic JSON Schema (no column schema).
    fn updatable_constraints_to_basic_json_schema(
        &self,
        constraints: &UpdatableColumnConstraints,
        col_name: &str,
    ) -> Value {
        let mut schema = json!({ "type": "string" });

        // Extract enum values from only_when if it's a simple new.<col>: [values] pattern
        if let Some(only_when) = &constraints.only_when
            && let Some(values) = only_when.get_new_value_restriction(col_name) {
                schema["enum"] = json!(values);
            }

        if let Some(guidance) = &constraints.guidance {
            schema["description"] = json!(guidance);
        }

        schema
    }
}

/// Convert a snake_case string to PascalCase.
fn pascal_case(s: &str) -> String {
    s.split('_')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                None => String::new(),
                Some(first) => first.to_uppercase().chain(chars).collect(),
            }
        })
        .collect()
}

/// Simple singularization (converts plural to singular).
fn singularize(s: &str) -> String {
    // Common irregular plurals
    let irregulars = [
        ("people", "person"),
        ("children", "child"),
        ("men", "man"),
        ("women", "woman"),
        ("mice", "mouse"),
        ("geese", "goose"),
        ("teeth", "tooth"),
        ("feet", "foot"),
    ];

    for (plural, singular) in irregulars {
        if s == plural {
            return singular.to_string();
        }
    }

    // Words ending in 'ies' -> 'y' (e.g., categories -> category)
    if s.ends_with("ies") && s.len() > 3 {
        return format!("{}y", &s[..s.len() - 3]);
    }

    // Words ending in 'es' that should keep the 'e' (e.g., boxes -> box)
    // Be careful about words like "status" that end in "us" not "es"
    if s.ends_with("xes") || s.ends_with("ches") || s.ends_with("shes") || s.ends_with("sses") {
        return s[..s.len() - 2].to_string();
    }

    // Words ending in 'ves' -> 'f' or 'fe' (e.g., leaves -> leaf)
    if s.ends_with("ves") {
        return format!("{}f", &s[..s.len() - 3]);
    }

    // Words ending in 's' but not 'ss' or 'us' or 'is' (e.g., users -> user)
    if s.ends_with('s') && !s.ends_with("ss") && !s.ends_with("us") && !s.ends_with("is") {
        return s[..s.len() - 1].to_string();
    }

    // Return as-is if no rule matched
    s.to_string()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::ColumnSchema;
    use cori_core::{TablePermissions, config::AllColumns};
    use std::collections::HashMap;

    #[test]
    fn test_generate_get_tool() {
        let mut role = RoleDefinition {
            name: "test".to_string(),
            description: None,
            approvals: None,
            tables: HashMap::new(),
        };

        role.tables.insert(
            "users".to_string(),
            TablePermissions {
                readable: ReadableConfig::List(vec!["id".to_string(), "name".to_string()]),
                creatable: CreatableColumns::default(),
                updatable: UpdatableColumns::default(),
                deletable: Default::default(),
            },
        );

        let mut schema = DatabaseSchema::new();
        let mut table = crate::schema::TableSchema::new("users");
        table.add_column(ColumnSchema::new("id", "integer"));
        table.add_column(ColumnSchema::new("name", "text"));
        table.primary_key.push("id".to_string());
        schema.add_table(table);

        let generator = ToolGenerator::new(role, schema);
        let tools = generator.generate_all();

        assert!(tools.iter().any(|t| t.name == "getUser"));
        assert!(tools.iter().any(|t| t.name == "listUsers"));
        // No create/update/delete since no creatable/updatable columns
        assert!(!tools.iter().any(|t| t.name == "createUser"));
    }

    #[test]
    fn test_generate_update_tool_with_constraints() {
        use cori_core::config::role_definition::{ColumnCondition, OnlyWhen};

        let mut role = RoleDefinition {
            name: "test".to_string(),
            description: None,
            approvals: None,
            tables: HashMap::new(),
        };

        // Create only_when with new.status: [values] pattern
        let mut status_conditions = HashMap::new();
        status_conditions.insert(
            "new.status".to_string(),
            ColumnCondition::In(vec![serde_json::json!("open"), serde_json::json!("closed")]),
        );

        let mut updatable = HashMap::new();
        updatable.insert(
            "status".to_string(),
            UpdatableColumnConstraints {
                only_when: Some(OnlyWhen::Single(status_conditions)),
                requires_approval: None,
                guidance: None,
                foreign_key: None,
            },
        );
        updatable.insert(
            "priority".to_string(),
            UpdatableColumnConstraints {
                requires_approval: Some(cori_core::ApprovalRequirement::Simple(true)),
                ..Default::default()
            },
        );

        role.tables.insert(
            "tickets".to_string(),
            TablePermissions {
                readable: ReadableConfig::List(vec!["id".to_string(), "status".to_string()]),
                creatable: CreatableColumns::default(),
                updatable: UpdatableColumns::Map(updatable),
                deletable: Default::default(),
            },
        );

        let mut schema = DatabaseSchema::new();
        let mut table = crate::schema::TableSchema::new("tickets");
        table.add_column(ColumnSchema::new("id", "integer"));
        table.add_column(ColumnSchema::new("status", "text"));
        table.add_column(ColumnSchema::new("priority", "text"));
        table.primary_key.push("id".to_string());
        schema.add_table(table);

        let generator = ToolGenerator::new(role, schema);
        let tools = generator.generate_all();

        let update_tool = tools.iter().find(|t| t.name == "updateTicket").unwrap();

        // Check that status has enum constraint
        let status_schema = &update_tool.input_schema["properties"]["status"];
        assert!(status_schema["enum"].is_array());

        // Check requires approval
        let annotations = update_tool.annotations.as_ref().unwrap();
        assert_eq!(annotations.requires_approval, Some(true));
    }

    #[test]
    fn test_no_get_update_delete_without_primary_key() {
        // Test that get, update, delete tools are NOT generated for tables without primary keys
        let mut role = RoleDefinition {
            name: "test".to_string(),
            description: None,
            approvals: None,
            tables: HashMap::new(),
        };

        // Create role with full permissions (readable, updatable, deletable)
        role.tables.insert(
            "logs".to_string(),
            TablePermissions {
                readable: ReadableConfig::List(vec![
                    "timestamp".to_string(),
                    "message".to_string(),
                ]),
                creatable: CreatableColumns::All(AllColumns),
                updatable: UpdatableColumns::All(AllColumns),
                deletable: cori_core::DeletablePermission::Allowed(true),
            },
        );

        // Create schema WITHOUT primary key
        let mut schema = DatabaseSchema::new();
        let mut table = crate::schema::TableSchema::new("logs");
        table.add_column(ColumnSchema::new("timestamp", "timestamp"));
        table.add_column(ColumnSchema::new("message", "text"));
        // Note: NO primary_key.push() - table has no PK
        schema.add_table(table);

        let generator = ToolGenerator::new(role, schema);
        let tools = generator.generate_all();

        // list should be generated (doesn't need PK)
        assert!(
            tools.iter().any(|t| t.name == "listLogs"),
            "listLogs should be generated"
        );

        // create should be generated (doesn't need PK)
        assert!(
            tools.iter().any(|t| t.name == "createLog"),
            "createLog should be generated"
        );

        // get should NOT be generated (needs PK for single record lookup)
        assert!(
            !tools.iter().any(|t| t.name == "getLog"),
            "getLog should NOT be generated without PK"
        );

        // update should NOT be generated (needs PK to identify row)
        assert!(
            !tools.iter().any(|t| t.name == "updateLog"),
            "updateLog should NOT be generated without PK"
        );

        // delete should NOT be generated (needs PK to identify row)
        assert!(
            !tools.iter().any(|t| t.name == "deleteLog"),
            "deleteLog should NOT be generated without PK"
        );
    }

    #[test]
    fn test_get_update_delete_with_primary_key() {
        // Test that get, update, delete tools ARE generated for tables WITH primary keys
        let mut role = RoleDefinition {
            name: "test".to_string(),
            description: None,
            approvals: None,
            tables: HashMap::new(),
        };

        role.tables.insert(
            "items".to_string(),
            TablePermissions {
                readable: ReadableConfig::List(vec!["id".to_string(), "name".to_string()]),
                creatable: CreatableColumns::All(AllColumns),
                updatable: UpdatableColumns::All(AllColumns),
                deletable: cori_core::DeletablePermission::Allowed(true),
            },
        );

        // Create schema WITH primary key
        let mut schema = DatabaseSchema::new();
        let mut table = crate::schema::TableSchema::new("items");
        table.add_column(ColumnSchema::new("id", "integer"));
        table.add_column(ColumnSchema::new("name", "text"));
        table.primary_key.push("id".to_string()); // Has PK
        schema.add_table(table);

        let generator = ToolGenerator::new(role, schema);
        let tools = generator.generate_all();

        // All tools should be generated when PK exists
        assert!(
            tools.iter().any(|t| t.name == "listItems"),
            "listItems should be generated"
        );
        assert!(
            tools.iter().any(|t| t.name == "createItem"),
            "createItem should be generated"
        );
        assert!(
            tools.iter().any(|t| t.name == "getItem"),
            "getItem should be generated with PK"
        );
        assert!(
            tools.iter().any(|t| t.name == "updateItem"),
            "updateItem should be generated with PK"
        );
        assert!(
            tools.iter().any(|t| t.name == "deleteItem"),
            "deleteItem should be generated with PK"
        );
    }
}
