//! `cori init` command implementation.
//!
//! Scaffolds a new Cori MCP server project by introspecting an existing
//! PostgreSQL database schema. Creates the project structure, configuration files,
//! Biscuit keypair, and sample role definitions.

use anyhow::{Context, Result};
use serde_json::Value as JsonValue;
use std::fs;
use std::path::PathBuf;

/// Well-known tenant column names (checked in priority order)
const TENANT_COLUMN_CANDIDATES: &[&str] = &[
    "tenant_id",
    "organization_id",
    "org_id",
    "customer_id",
    "account_id",
    "client_id",
];

/// Run the `cori init` command.
///
/// # Arguments
/// * `database_url` - PostgreSQL connection URL
/// * `project` - Project name (used as output directory)
/// * `force` - Overwrite existing directory if true
pub async fn run(database_url: &str, project: &str, force: bool) -> Result<()> {
    let project_dir = PathBuf::from(project);

    // Check if project directory exists
    if project_dir.exists() {
        if !force {
            anyhow::bail!(
                "Project directory '{}' already exists. Use --force to overwrite.",
                project
            );
        }
        println!("⚠️  Overwriting existing project directory...");
    }

    println!("🚀 Initializing Cori project: {}", project);
    println!();

    // Verify database connectivity before creating any files
    println!("🔌 Checking database connectivity...");
    check_database_connectivity(database_url)
        .await
        .context("Database connection failed. Please verify the database URL and ensure the database is running.")?;
    println!("   ✓ Database is reachable");

    // Create directory structure
    println!("📁 Creating project structure...");
    create_project_structure(&project_dir)?;

    // Generate Biscuit keypair
    println!("🔑 Generating Biscuit keypair...");
    let keys_dir = project_dir.join("keys");
    generate_keypair(&keys_dir)?;

    // Introspect database schema
    println!("🔍 Introspecting database schema...");
    let schema = cori_adapter_pg::introspect::introspect_schema_json(database_url)
        .await
        .context("Failed to introspect database schema")?;

    // Analyze schema for tenant columns and FK relationships
    let tables = extract_tables(&schema);
    let tenant_column = detect_tenant_column(&tables);
    println!("   ✓ Detected {} tables", tables.len());

    // Count FK relationships for reporting
    let total_fks: usize = tables.iter().map(|t| t.foreign_keys.len()).sum();
    if total_fks > 0 {
        println!("   ✓ Detected {} foreign key relationships", total_fks);
    }

    if let Some(ref col) = tenant_column {
        println!("   ✓ Auto-detected tenant column: {}", col);
    } else {
        println!("   ⚠ No tenant column auto-detected (configure manually in schema/rules.yaml)");
    }

    // Analyze tenancy including FK chain resolution
    println!("🔗 Analyzing tenancy and FK relationships...");
    let tenancy_analysis = analyze_tenancy(&tables, &tenant_column);

    // Report tenancy findings
    let direct_count = tenancy_analysis
        .table_tenancy
        .values()
        .filter(|s| matches!(s, TenancySource::Direct { .. }))
        .count();
    let fk_count = tenancy_analysis
        .table_tenancy
        .values()
        .filter(|s| matches!(s, TenancySource::ViaForeignKey { .. }))
        .count();
    let global_count = tenancy_analysis
        .table_tenancy
        .values()
        .filter(|s| matches!(s, TenancySource::Global))
        .count();
    let unknown_count = tenancy_analysis
        .table_tenancy
        .values()
        .filter(|s| matches!(s, TenancySource::Unknown))
        .count();

    println!("   ✓ {} tables with direct tenant column", direct_count);
    if fk_count > 0 {
        println!(
            "   ✓ {} tables with tenant via FK chain (will use JOINs)",
            fk_count
        );
    }
    if global_count > 0 {
        println!("   ✓ {} global tables (no tenant isolation)", global_count);
    }
    if unknown_count > 0 {
        println!(
            "   ⚠ {} tables need manual tenancy configuration",
            unknown_count
        );
    }

    // Print FK chain warnings
    for warning in &tenancy_analysis.warnings {
        println!("   {}", warning);
    }

    // Note: Tables not listed in roles are implicitly blocked
    // No separate sensitive_tables analysis needed

    // Generate schema files
    println!("📄 Generating schema files...");
    let schema_dir = project_dir.join("schema");

    // 1. schema/schema.yaml - Auto-generated database schema (SchemaDefinition.schema.json)
    let schema_yaml = generate_schema_yaml(&schema, &tables);
    fs::write(schema_dir.join("schema.yaml"), &schema_yaml)?;
    println!("   ✓ schema/schema.yaml (auto-generated database schema)");

    // 2. schema/rules.yaml - User-editable tenancy and validation rules (RulesDefinition.schema.json)
    let rules_yaml = generate_rules_yaml(&tenancy_analysis, &tables);
    fs::write(schema_dir.join("rules.yaml"), &rules_yaml)?;
    println!("   ✓ schema/rules.yaml (tenancy, soft-delete, validation rules)");

    // 3. schema/types.yaml - Reusable semantic types (TypesDefinition.schema.json)
    let types_yaml = generate_types_yaml();
    fs::write(schema_dir.join("types.yaml"), &types_yaml)?;
    println!("   ✓ schema/types.yaml (reusable semantic types)");

    // Generate configuration file (CoriDefinition.schema.json)
    println!("⚙️  Generating configuration...");
    let config = generate_config(project);
    let config_path = project_dir.join("cori.yaml");
    fs::write(&config_path, config)?;
    println!("   ✓ cori.yaml (main configuration)");

    // Generate sample roles (RoleDefinition.schema.json)
    println!("👥 Generating sample roles...");
    let roles_dir = project_dir.join("roles");
    generate_sample_roles(&roles_dir, &tables, &tenant_column)?;
    println!("   ✓ Sample roles written to roles/");

    // Generate sample approval group (GroupDefinition.schema.json)
    println!("👥 Generating sample approval group...");
    let groups_dir = project_dir.join("groups");
    generate_sample_groups(&groups_dir)?;
    println!("   ✓ Sample group written to groups/");

    // Generate README
    let readme_path = project_dir.join("README.md");
    let readme = generate_readme(project, &tenant_column, &tenancy_analysis);
    fs::write(&readme_path, readme)?;
    println!("   ✓ README.md generated");

    // Print summary
    println!();
    println!("✅ Cori project initialized successfully!");
    println!();
    println!("📂 Project structure:");
    println!("   {}/", project);
    println!("   ├── cori.yaml           # Main configuration file");
    println!("   ├── .gitignore          # Ignores keys and sensitive files");
    println!("   ├── README.md           # Project-specific getting started guide");
    println!("   ├── keys/");
    println!("   │   ├── private.key     # Ed25519 private key (keep secure!)");
    println!("   │   └── public.key      # Ed25519 public key");
    println!("   ├── roles/");
    println!("   │   └── *.yaml          # Auto-generated role definitions");
    println!("   ├── groups/");
    println!("   │   └── *.yaml          # Sample approval groups");
    println!("   ├── schema/");
    println!("   │   ├── schema.yaml     # Auto-generated database schema (DO NOT EDIT)");
    println!("   │   ├── rules.yaml      # Tenancy, soft-delete, validation rules");
    println!("   │   └── types.yaml      # Reusable semantic types");
    println!("   └── tokens/             # Generated tokens (gitignored)");
    println!();
    println!("🚦 Next steps:");
    println!("   1. cd {}", project);
    println!("   2. Review schema/rules.yaml - especially tables with FK-inherited tenancy");
    println!("   3. Review and customize roles in roles/*.yaml");
    println!("   4. Set DATABASE_URL environment variable");
    println!("   5. Start the server: cori run");
    println!("   6. Mint a token: cori token mint --role support_agent --output role.token");
    println!(
        "   7. Attenuate for tenant: cori token attenuate --base role.token --tenant <id> --output agent.token"
    );
    println!();
    println!("📚 See README.md for detailed instructions.");

    Ok(())
}

/// Check that the database exists and is reachable.
///
/// This is called before creating any project files to ensure we don't
/// leave behind partial project structures when the database is unavailable.
async fn check_database_connectivity(database_url: &str) -> Result<()> {
    use sqlx::PgPool;

    let pool = PgPool::connect(database_url)
        .await
        .context("Failed to connect to database")?;

    // Run a simple query to verify the connection is working
    sqlx::query("SELECT 1")
        .fetch_one(&pool)
        .await
        .context("Failed to execute test query")?;

    pool.close().await;

    Ok(())
}

/// Create the project directory structure.
///
/// Creates the following structure:
/// - schema/           # Database schema files
/// - roles/            # Role definitions (one file per role)
/// - groups/           # Approval groups
/// - keys/             # Biscuit keypair
/// - tokens/           # Generated tokens (gitignored)
fn create_project_structure(project_dir: &PathBuf) -> Result<()> {
    let dirs = ["schema", "roles", "groups", "keys", "tokens"];

    for dir in dirs {
        fs::create_dir_all(project_dir.join(dir))?;
    }

    // Create .gitignore for sensitive files
    let gitignore = r#"# Cori project gitignore

# Private keys - NEVER commit these!
keys/private.key

# Generated tokens
tokens/
*.token
*.biscuit

# Environment files with secrets
.env
.env.*

# Local development
*.local.yaml

# Logs
logs/
"#;
    fs::write(project_dir.join(".gitignore"), gitignore)?;

    Ok(())
}

/// Generate Biscuit Ed25519 keypair.
fn generate_keypair(keys_dir: &PathBuf) -> Result<()> {
    use cori_biscuit::KeyPair;

    let keypair = KeyPair::generate()?;

    let private_key = keypair.private_key_hex();
    let public_key = keypair.public_key_hex();

    fs::write(keys_dir.join("private.key"), &private_key)?;
    fs::write(keys_dir.join("public.key"), &public_key)?;

    Ok(())
}

/// Extract table information from schema snapshot.
fn extract_tables(schema: &JsonValue) -> Vec<TableInfo> {
    let mut tables = Vec::new();

    if let Some(table_array) = schema.get("tables").and_then(|t| t.as_array()) {
        for table in table_array {
            let name = table
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or_default()
                .to_string();
            let schema_name = table
                .get("schema")
                .and_then(|s| s.as_str())
                .unwrap_or("public")
                .to_string();

            let columns: Vec<ColumnInfo> = table
                .get("columns")
                .and_then(|c| c.as_array())
                .map(|cols| {
                    cols.iter()
                        .filter_map(|c| {
                            let col_name = c.get("name")?.as_str()?.to_string();
                            // Use native_type to match introspect output (SchemaDefinition.schema.json)
                            let data_type = c.get("native_type")?.as_str()?.to_string();
                            let nullable =
                                c.get("nullable").and_then(|n| n.as_bool()).unwrap_or(true);
                            Some(ColumnInfo {
                                name: col_name,
                                data_type,
                                nullable,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();

            let primary_key: Vec<String> = table
                .get("primary_key")
                .and_then(|pk| pk.as_array())
                .map(|pks| {
                    pks.iter()
                        .filter_map(|p| p.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();

            // Extract foreign key information
            let foreign_keys: Vec<ForeignKeyInfo> = table
                .get("foreign_keys")
                .and_then(|fks| fks.as_array())
                .map(|fk_list| {
                    fk_list
                        .iter()
                        .flat_map(|fk| {
                            let fk_name = fk
                                .get("name")
                                .and_then(|n| n.as_str())
                                .unwrap_or_default()
                                .to_string();
                            fk.get("mappings")
                                .and_then(|m| m.as_array())
                                .map(|mappings| {
                                    mappings
                                        .iter()
                                        .filter_map(|mapping| {
                                            let column =
                                                mapping.get("column")?.as_str()?.to_string();
                                            let refs = mapping.get("references")?;
                                            let ref_table =
                                                refs.get("table")?.as_str()?.to_string();
                                            let ref_column =
                                                refs.get("column")?.as_str()?.to_string();
                                            Some(ForeignKeyInfo {
                                                name: fk_name.clone(),
                                                column,
                                                references_table: ref_table,
                                                references_column: ref_column,
                                            })
                                        })
                                        .collect::<Vec<_>>()
                                })
                                .unwrap_or_default()
                        })
                        .collect()
                })
                .unwrap_or_default();

            tables.push(TableInfo {
                schema: schema_name,
                name,
                columns,
                primary_key,
                foreign_keys,
            });
        }
    }

    tables
}

/// Detect the most likely tenant column across all tables.
fn detect_tenant_column(tables: &[TableInfo]) -> Option<String> {
    for candidate in TENANT_COLUMN_CANDIDATES {
        let count = tables
            .iter()
            .filter(|t| t.columns.iter().any(|c| c.name == *candidate))
            .count();

        // If at least half the tables have this column, it's likely the tenant column
        if count > 0 && count >= tables.len() / 2 {
            return Some(candidate.to_string());
        }
    }

    // Also check for columns containing "tenant", "org", etc.
    let mut column_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for table in tables {
        for col in &table.columns {
            let lower = col.name.to_lowercase();
            if lower.contains("tenant")
                || lower.contains("org")
                || (lower.contains("account") && lower.contains("id"))
            {
                *column_counts.entry(col.name.clone()).or_insert(0) += 1;
            }
        }
    }

    column_counts
        .into_iter()
        .filter(|(_, count)| *count >= tables.len() / 2)
        .max_by_key(|(_, count)| *count)
        .map(|(name, _)| name)
}

/// Analyze tenancy for all tables, detecting both direct tenant columns and FK chains.
///
/// This function:
/// 1. Identifies tables with direct tenant columns
/// 2. For tables without direct tenant columns, traces FK relationships to find tenant context
/// 3. Highlights tables that need manual configuration or review
fn analyze_tenancy(tables: &[TableInfo], tenant_column: &Option<String>) -> TenancyAnalysis {
    use std::collections::HashMap;

    let tenant_col = tenant_column
        .clone()
        .unwrap_or_else(|| "organization_id".to_string());

    // Build a lookup map for tables
    let table_map: HashMap<&str, &TableInfo> =
        tables.iter().map(|t| (t.name.as_str(), t)).collect();

    let mut table_tenancy: HashMap<String, TenancySource> = HashMap::new();
    let mut warnings: Vec<String> = Vec::new();

    // First pass: identify tables with direct tenant columns
    for table in tables {
        if let Some(col) = table.columns.iter().find(|c| c.name == tenant_col) {
            table_tenancy.insert(
                table.name.clone(),
                TenancySource::Direct {
                    column: tenant_col.clone(),
                    data_type: normalize_tenant_type(&col.data_type),
                },
            );
        }
    }

    // Second pass: for tables without direct tenant columns, trace FK chains
    for table in tables {
        if table_tenancy.contains_key(&table.name) {
            continue; // Already has direct tenant column
        }

        // Try to find tenant context via FK chain
        match find_tenant_via_fk(table, &table_map, &tenant_col, &table_tenancy) {
            Some(source) => {
                if let TenancySource::ViaForeignKey { chain, .. } = &source {
                    let chain_desc: Vec<String> =
                        chain.iter().map(|(t, c)| format!("{}.{}", t, c)).collect();
                    warnings.push(format!(
                        "Table '{}' has tenant context via FK chain: {} → {}",
                        table.name,
                        table.name,
                        chain_desc.join(" → ")
                    ));
                }
                table_tenancy.insert(table.name.clone(), source);
            }
            None => {
                // Check if this looks like a global/reference table
                if is_likely_global_table(&table.name, table) {
                    table_tenancy.insert(table.name.clone(), TenancySource::Global);
                } else {
                    warnings.push(format!(
                        "⚠️  Table '{}' has no tenant column '{}' and no FK path to tenant. \
                         Configure as 'global: true' or add tenant_column manually.",
                        table.name, tenant_col
                    ));
                    table_tenancy.insert(table.name.clone(), TenancySource::Unknown);
                }
            }
        }
    }

    TenancyAnalysis {
        tenant_column: Some(tenant_col),
        table_tenancy,
        warnings,
    }
}

/// Normalize SQL data type to tenancy type (uuid, integer, string)
fn normalize_tenant_type(data_type: &str) -> String {
    let lower = data_type.to_lowercase();
    if lower.contains("uuid") {
        "uuid".to_string()
    } else if lower.contains("int") {
        "integer".to_string()
    } else {
        "string".to_string()
    }
}

/// Try to find tenant context for a table via foreign key chains.
/// Uses BFS to find the shortest path to a table with a direct tenant column.
fn find_tenant_via_fk(
    table: &TableInfo,
    table_map: &std::collections::HashMap<&str, &TableInfo>,
    _tenant_col: &str,
    direct_tenant_tables: &std::collections::HashMap<String, TenancySource>,
) -> Option<TenancySource> {
    use std::collections::{HashSet, VecDeque};

    // BFS to find shortest FK path to a tenant-aware table
    // Queue items: (current_table_name, fk_column_in_original_table, path_so_far)
    let mut queue: VecDeque<(String, String, Vec<(String, String)>)> = VecDeque::new();
    let mut visited: HashSet<String> = HashSet::new();

    // Initialize with direct FKs from this table
    for fk in &table.foreign_keys {
        queue.push_back((
            fk.references_table.clone(),
            fk.column.clone(),
            vec![(fk.references_table.clone(), fk.references_column.clone())],
        ));
    }
    visited.insert(table.name.clone());

    // Limit chain length to prevent infinite loops and overly complex chains
    const MAX_CHAIN_LENGTH: usize = 3;

    while let Some((current_table, fk_column, path)) = queue.pop_front() {
        if path.len() > MAX_CHAIN_LENGTH {
            continue;
        }

        if visited.contains(&current_table) {
            continue;
        }
        visited.insert(current_table.clone());

        // Check if this table has a direct tenant column
        if let Some(TenancySource::Direct { column, .. }) = direct_tenant_tables.get(&current_table)
        {
            return Some(TenancySource::ViaForeignKey {
                fk_column,
                chain: path,
                tenant_column: column.clone(),
            });
        }

        // Continue traversing FKs from this table
        if let Some(ref_table) = table_map.get(current_table.as_str()) {
            for fk in &ref_table.foreign_keys {
                if !visited.contains(&fk.references_table) {
                    let mut new_path = path.clone();
                    new_path.push((fk.references_table.clone(), fk.references_column.clone()));
                    queue.push_back((fk.references_table.clone(), fk_column.clone(), new_path));
                }
            }
        }
    }

    None
}

/// Heuristic to detect likely global/reference tables that don't need tenant isolation.
fn is_likely_global_table(name: &str, table: &TableInfo) -> bool {
    let lower = name.to_lowercase();

    // Common global/reference table patterns
    let global_patterns = [
        "country",
        "countries",
        "currency",
        "currencies",
        "language",
        "languages",
        "timezone",
        "timezones",
        "region",
        "regions",
        "status",
        "statuses",
        "type",
        "types",
        "category",
        "categories",
        "permission",
        "permissions",
        "role",
        "roles",
        "setting",
        "settings",
        "config",
        "configuration",
        "enum",
        "lookup",
        "reference",
        "ref_",
        "_ref",
        "master_",
        "_master",
    ];

    // Check name patterns
    if global_patterns.iter().any(|p| lower.contains(p)) {
        return true;
    }

    // Small tables with no FKs are often lookup tables
    if table.foreign_keys.is_empty() && table.columns.len() <= 5 {
        return true;
    }

    false
}

/// Generate schema/schema.yaml content matching SchemaDefinition.schema.json.
/// This is an auto-generated file that captures the database structure.
fn generate_schema_yaml(raw_schema: &JsonValue, tables: &[TableInfo]) -> String {
    let now = chrono::Utc::now().to_rfc3339();

    // Extract database info
    let db_version = raw_schema
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    // Extract enums
    let enums_yaml = raw_schema
        .get("enums")
        .and_then(|e| e.as_array())
        .map(|enums| {
            enums
                .iter()
                .filter_map(|e| {
                    let name = e.get("name")?.as_str()?;
                    let schema = e.get("schema").and_then(|s| s.as_str()).unwrap_or("public");
                    let values: Vec<&str> = e
                        .get("values")?
                        .as_array()?
                        .iter()
                        .filter_map(|v| v.as_str())
                        .collect();
                    Some(format!(
                        "  - name: {}\n    schema: {}\n    values: [{}]",
                        name,
                        schema,
                        values.join(", ")
                    ))
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();

    // Generate table entries
    let tables_yaml: Vec<String> = tables
        .iter()
        .map(|table| {
            let columns_yaml: Vec<String> = table
                .columns
                .iter()
                .map(|col| {
                    let generic_type = normalize_column_type(&col.data_type);
                    let nullable = if col.nullable { "true" } else { "false" };
                    format!(
                        "      - name: {}\n        type: {}\n        native_type: \"{}\"\n        nullable: {}",
                        col.name, generic_type, col.data_type, nullable
                    )
                })
                .collect();

            let pk_yaml = if table.primary_key.is_empty() {
                String::new()
            } else {
                format!("    primary_key: [{}]\n", table.primary_key.join(", "))
            };

            let fk_yaml = if table.foreign_keys.is_empty() {
                String::new()
            } else {
                let fks: Vec<String> = table
                    .foreign_keys
                    .iter()
                    .map(|fk| {
                        format!(
                            "      - name: {}\n        columns: [{}]\n        references:\n          table: {}\n          columns: [{}]",
                            fk.name, fk.column, fk.references_table, fk.references_column
                        )
                    })
                    .collect();
                format!("    foreign_keys:\n{}\n", fks.join("\n"))
            };

            format!(
                "  - name: {}\n    schema: {}\n    columns:\n{}\n{}{}",
                table.name,
                table.schema,
                columns_yaml.join("\n"),
                pk_yaml,
                fk_yaml
            )
        })
        .collect();

    format!(
        r#"# =============================================================================
# Schema Definition
# =============================================================================
# Auto-generated database schema. DO NOT EDIT manually.
# Run 'cori db sync' to update from live database.
# Conforms to: SchemaDefinition.schema.json
# =============================================================================

version: "1.0.0"
captured_at: "{now}"

database:
  engine: postgres
  version: "{db_version}"

{enums_section}
tables:
{tables}
"#,
        now = now,
        db_version = db_version,
        enums_section = if enums_yaml.is_empty() {
            String::new()
        } else {
            format!("enums:\n{}\n\n", enums_yaml)
        },
        tables = tables_yaml.join("\n")
    )
}

/// Normalize SQL data type to generic schema type (matching SchemaDefinition.schema.json)
fn normalize_column_type(data_type: &str) -> &'static str {
    let lower = data_type.to_lowercase();
    if lower.contains("uuid") {
        "uuid"
    } else if lower.contains("serial") || lower.contains("bigint") {
        "bigint"
    } else if lower.contains("int") {
        "integer"
    } else if lower.contains("numeric") || lower.contains("decimal") {
        "decimal"
    } else if lower.contains("float") || lower.contains("double") || lower.contains("real") {
        "float"
    } else if lower.contains("bool") {
        "boolean"
    } else if lower.contains("timestamp") {
        "timestamp"
    } else if lower.contains("date") {
        "date"
    } else if lower.contains("time") {
        "time"
    } else if lower.contains("jsonb") {
        "jsonb"
    } else if lower.contains("json") {
        "json"
    } else if lower.contains("text") {
        "text"
    } else if lower.contains("varchar") || lower.contains("char") {
        "string"
    } else if lower.contains("bytea") || lower.contains("binary") {
        "binary"
    } else {
        "unknown"
    }
}

/// Generate schema/rules.yaml content matching RulesDefinition.schema.json.
/// This defines tenancy, soft-delete, and validation rules.
fn generate_rules_yaml(analysis: &TenancyAnalysis, tables: &[TableInfo]) -> String {
    let tenant_col = analysis
        .tenant_column
        .clone()
        .unwrap_or_else(|| "organization_id".to_string());

    let mut table_entries = Vec::new();

    // Sort tables for consistent output
    let mut sorted_tables: Vec<_> = analysis.table_tenancy.iter().collect();
    sorted_tables.sort_by_key(|(name, _)| name.as_str());

    for (table_name, source) in sorted_tables {
        // Find the original table info for column details
        let table_info = tables.iter().find(|t| &t.name == table_name);

        let entry = match source {
            TenancySource::Direct { column, .. } => {
                let columns_yaml = generate_column_rules(table_info);
                format!(
                    r#"  {}:
    tenant: {}{}
"#,
                    table_name, column, columns_yaml
                )
            }
            TenancySource::ViaForeignKey {
                fk_column, chain, ..
            } => {
                let ref_table = &chain[0].0;
                let columns_yaml = generate_column_rules(table_info);
                format!(
                    r#"  {}:
    tenant:
      via: {}
      references: {}{}
"#,
                    table_name, fk_column, ref_table, columns_yaml
                )
            }
            TenancySource::Global => {
                let columns_yaml = generate_column_rules(table_info);
                format!(
                    r#"  {}:
    global: true{}
"#,
                    table_name, columns_yaml
                )
            }
            TenancySource::Unknown => {
                let columns_yaml = generate_column_rules(table_info);
                format!(
                    r#"  # {}: 
    # ⚠️ NEEDS CONFIGURATION - no tenant column or FK path detected
    # Uncomment one of the following options:
    # tenant: {}           # If this table has the tenant column
    # tenant:
    #   via: <fk_column>   # If tenant is inherited via FK
    #   references: <parent_table>
    # global: true         # If this is shared reference data{}
"#,
                    table_name, tenant_col, columns_yaml
                )
            }
        };
        table_entries.push(entry);
    }

    format!(
        r#"# =============================================================================
# Rules Definition
# =============================================================================
# User-modifiable column rules for tenancy, validation, and soft-delete.
# Conforms to: RulesDefinition.schema.json
#
# This file defines:
#   - Which column holds tenant IDs in each table (tenant: <column>)
#   - Inherited tenancy via FK relationships (tenant.via + tenant.references)
#   - Global tables shared across tenants (global: true)
#   - Soft-delete configuration (soft_delete.column)
#   - Column validation and tagging (columns.<name>.type, tags)
# =============================================================================

version: "1.0.0"

tables:
{tables}
"#,
        tables = table_entries.join("")
    )
}

/// Generate column rules for a table (PII detection, etc.)
fn generate_column_rules(table_info: Option<&TableInfo>) -> String {
    let Some(table) = table_info else {
        return String::new();
    };

    let mut column_rules = Vec::new();

    for col in &table.columns {
        let lower = col.name.to_lowercase();
        let mut rules = Vec::new();

        // Detect email columns
        if lower == "email" || lower.ends_with("_email") {
            rules.push("type: email".to_string());
            rules.push("tags: [pii]".to_string());
        }
        // Detect phone columns
        else if lower == "phone" || lower.ends_with("_phone") || lower == "phone_number" {
            rules.push("type: phone".to_string());
            rules.push("tags: [pii]".to_string());
        }
        // Detect SSN/sensitive ID columns
        else if lower == "ssn" || lower.contains("social_security") {
            rules.push("tags: [pii, sensitive]".to_string());
        }
        // Detect address columns
        else if lower.contains("address") && !lower.contains("ip") {
            rules.push("tags: [pii]".to_string());
        }
        // Detect status columns with common values
        else if lower == "status" {
            rules.push("allowed_values: [active, inactive, pending, archived]".to_string());
        }

        if !rules.is_empty() {
            column_rules.push(format!(
                "      {}:\n        {}",
                col.name,
                rules.join("\n        ")
            ));
        }
    }

    // Check for soft-delete columns
    let soft_delete_col = table.columns.iter().find(|c| {
        let lower = c.name.to_lowercase();
        lower == "deleted_at" || lower == "is_deleted" || lower == "deleted"
    });

    let soft_delete_yaml = soft_delete_col.map(|col| {
        let lower = col.name.to_lowercase();
        if lower == "deleted_at" {
            format!(
                "\n    soft_delete:\n      column: {}\n      deleted_value: \"NOW()\"\n      active_value: null",
                col.name
            )
        } else {
            format!(
                "\n    soft_delete:\n      column: {}\n      deleted_value: true\n      active_value: false",
                col.name
            )
        }
    }).unwrap_or_default();

    if column_rules.is_empty() && soft_delete_yaml.is_empty() {
        String::new()
    } else {
        let columns_section = if column_rules.is_empty() {
            String::new()
        } else {
            format!("\n    columns:\n{}", column_rules.join("\n"))
        };
        format!("{}{}", soft_delete_yaml, columns_section)
    }
}

/// Generate schema/types.yaml content matching TypesDefinition.schema.json.
/// This defines reusable semantic types for validation.
fn generate_types_yaml() -> String {
    r#"# =============================================================================
# Types Definition
# =============================================================================
# Reusable semantic types for input validation.
# Referenced by column rules in schema/rules.yaml.
# Conforms to: TypesDefinition.schema.json
# =============================================================================

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

  url:
    description: "HTTP/HTTPS URL"
    pattern: "^https?://[^\\s]+$"

  uuid:
    description: "UUID v4 format"
    pattern: "^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$"

  slug:
    description: "URL-safe slug"
    pattern: "^[a-z0-9]+(?:-[a-z0-9]+)*$"

  currency_code:
    description: "ISO 4217 currency code"
    pattern: "^[A-Z]{3}$"

  country_code:
    description: "ISO 3166-1 alpha-2 country code"
    pattern: "^[A-Z]{2}$"

  # Sensitive types - always tagged for special handling
  ssn:
    description: "Social Security Number (US)"
    pattern: "^\\d{3}-\\d{2}-\\d{4}$"
    tags: [pii, sensitive]

  credit_card:
    description: "Credit card number (basic validation)"
    pattern: "^\\d{13,19}$"
    tags: [pii, sensitive]
"#
    .to_string()
}

/// Generate a sample approval group (GroupDefinition.schema.json)
fn generate_sample_groups(groups_dir: &PathBuf) -> Result<()> {
    let support_managers = r#"# =============================================================================
# Approval Group: Support Managers
# =============================================================================
# Managers who can approve support ticket priority changes and escalations.
# Conforms to: GroupDefinition.schema.json
# =============================================================================

name: support_managers
description: "Managers who can approve support ticket priority changes"
members:
  - manager@example.com
  - lead@example.com
  # Add actual team member emails here
"#;

    fs::write(groups_dir.join("support_managers.yaml"), support_managers)?;
    Ok(())
}

/// Generate the cori.yaml configuration file content.
/// Conforms to CoriDefinition.schema.json
fn generate_config(project: &str) -> String {
    format!(
        r#"# =============================================================================
# Cori Configuration
# =============================================================================
# Project: {project}
# Generated by: cori init
# Conforms to: CoriDefinition.schema.json
#
# This is the main configuration file for a Cori Store.
# It defines database connection, Biscuit tokens, MCP server, and audit logging.
#
# Convention over configuration:
#   - Schema files are loaded from schema/ directory
#   - Role files are loaded from roles/ directory  
#   - Group files are loaded from groups/ directory
#   - Keys are loaded from keys/ directory
# =============================================================================

project: {project}
version: "1.0"

# =============================================================================
# UPSTREAM DATABASE
# =============================================================================
# The PostgreSQL database that Cori protects.
# Three configuration methods supported (choose one):

upstream:
  # Method 1: Environment variable (RECOMMENDED for production)
  database_url_env: DATABASE_URL
  
  # Method 2: Direct URL (for development only - don't commit secrets!)
  # database_url: postgres://user:pass@localhost:5432/mydb
  
  # Method 3: Individual fields
  # host: localhost
  # port: 5432
  # database: mydb
  # username: postgres
  # password_env: POSTGRES_PASSWORD  # Reference env var for password
  
  # Optional: SSL and connection pool settings
  # ssl_mode: prefer  # disable, allow, prefer, require, verify-ca, verify-full
  # pool:
  #   min_connections: 1
  #   max_connections: 10
  #   acquire_timeout_seconds: 30
  #   idle_timeout_seconds: 600

# =============================================================================
# BISCUIT TOKEN CONFIGURATION
# =============================================================================
# Authentication via cryptographic Biscuit tokens.
# Keys are auto-generated in keys/ directory during init.

biscuit:
  public_key_file: keys/public.key
  private_key_file: keys/private.key
  # Or via environment variables (for production):
  # public_key_env: BISCUIT_PUBLIC_KEY
  # private_key_env: BISCUIT_PRIVATE_KEY

# =============================================================================
# MCP SERVER (Model Context Protocol)
# =============================================================================
# Expose database actions as typed tools for AI agents.

mcp:
  enabled: true
  transport: stdio  # For Claude Desktop and local agents
  
  # For HTTP transport (remote agents, API integrations):
  # transport: http
  # host: 127.0.0.1
  # port: 3000

# =============================================================================
# ADMIN DASHBOARD
# =============================================================================

dashboard:
  enabled: true
  host: 127.0.0.1
  port: 8080
  auth:
    type: basic
    users:
      - username: admin
        password_env: DASHBOARD_ADMIN_PASSWORD

# =============================================================================
# AUDIT LOGGING
# =============================================================================

audit:
  enabled: true
  directory: logs/
  stdout: false
  retention_days: 90

# =============================================================================
# OBSERVABILITY (optional - not yet implemented)
# =============================================================================

# observability:
#   metrics:
#     enabled: true
#     port: 9090
#     path: /metrics
#   health:
#     enabled: true
#     port: 8081
#     path: /health
"#,
        project = project,
    )
}

/// Generate sample role definition files.
fn generate_sample_roles(
    roles_dir: &PathBuf,
    tables: &[TableInfo],
    tenant_column: &Option<String>,
) -> Result<()> {
    // All tables are accessible - tables not listed in a role are implicitly blocked
    let accessible_tables: Vec<&TableInfo> = tables.iter().collect();

    // Generate admin role (full access)
    let admin_role = generate_admin_role(tables, tenant_column);
    fs::write(roles_dir.join("admin_agent.yaml"), admin_role)?;

    // Generate readonly role (read all tables)
    let readonly_role = generate_readonly_role(&accessible_tables);
    fs::write(roles_dir.join("readonly_agent.yaml"), readonly_role)?;

    // Generate support role (common customer-facing tables)
    let support_role = generate_support_role(&accessible_tables);
    fs::write(roles_dir.join("support_agent.yaml"), support_role)?;

    Ok(())
}

/// Generate admin role with full access.
/// Conforms to RoleDefinition.schema.json
fn generate_admin_role(tables: &[TableInfo], _tenant_column: &Option<String>) -> String {
    let table_entries: Vec<String> = tables
        .iter()
        .map(|t| {
            format!(
                r#"  {}:
    readable: "*"
    creatable: "*"
    updatable: "*"
    deletable: true"#,
                t.name
            )
        })
        .collect();

    format!(
        r#"# =============================================================================
# ADMIN AGENT ROLE
# =============================================================================
# Full administrative access to all tables and columns.
# Use with extreme caution - typically for internal tools only.
# Note: All tables are listed - omit any tables you want to restrict.
# Conforms to: RoleDefinition.schema.json
# =============================================================================

name: admin_agent
description: "Administrative agent with full database access"

tables:
{}
"#,
        table_entries.join("\n\n")
    )
}

/// Generate readonly role.
/// Conforms to RoleDefinition.schema.json
fn generate_readonly_role(tables: &[&TableInfo]) -> String {
    let table_entries: Vec<String> = tables
        .iter()
        .take(20) // Limit to first 20 tables for readability
        .map(|t| {
            let columns: Vec<String> = t.columns.iter().map(|c| c.name.clone()).collect();
            format!(
                r#"  {}:
    readable: [{}]
    # No creatable, updatable, or deletable = read-only"#,
                t.name,
                columns.join(", ")
            )
        })
        .collect();

    format!(
        r#"# =============================================================================
# READONLY AGENT ROLE
# =============================================================================
# Read-only access to non-sensitive tables.
# Suitable for analytics and reporting agents.
# Note: Tables not listed here are implicitly blocked.
# Conforms to: RoleDefinition.schema.json
# =============================================================================

name: readonly_agent
description: "Read-only agent for analytics and reporting"

tables:
{}
"#,
        table_entries.join("\n\n")
    )
}

/// Generate support agent role.
/// Conforms to RoleDefinition.schema.json
fn generate_support_role(tables: &[&TableInfo]) -> String {
    // Common customer-facing table patterns
    let customer_tables = [
        "customers",
        "customer",
        "contacts",
        "contact",
        "clients",
        "client",
    ];
    let ticket_tables = [
        "tickets",
        "ticket",
        "issues",
        "issue",
        "support_tickets",
        "cases",
        "case",
    ];
    let order_tables = ["orders", "order", "purchases", "transactions"];
    let common_tables = [
        "products", "product", "services", "service", "notes", "comments",
    ];

    let all_patterns: Vec<&str> = customer_tables
        .iter()
        .chain(ticket_tables.iter())
        .chain(order_tables.iter())
        .chain(common_tables.iter())
        .copied()
        .collect();

    let support_tables: Vec<&&TableInfo> = tables
        .iter()
        .filter(|t| {
            let lower = t.name.to_lowercase();
            all_patterns.iter().any(|p| lower.contains(p))
        })
        .collect();

    let table_entries: Vec<String> = if support_tables.is_empty() {
        // If no common tables found, use first few tables (read-only)
        tables
            .iter()
            .take(5)
            .map(|t| {
                let columns: Vec<String> = t.columns.iter().map(|c| c.name.clone()).collect();
                format!(
                    r#"  {}:
    readable: [{}]
    # No creatable, updatable, or deletable = read-only"#,
                    t.name,
                    columns.join(", ")
                )
            })
            .collect()
    } else {
        support_tables
            .iter()
            .map(|t| {
                let columns: Vec<String> = t.columns.iter().map(|c| c.name.clone()).collect();
                // Check if this looks like a tickets table (allow status updates)
                let is_ticket = ticket_tables
                    .iter()
                    .any(|p| t.name.to_lowercase().contains(p));

                if is_ticket && t.columns.iter().any(|c| c.name == "status") {
                    // Ticket table with status - allow updating
                    format!(
                        r#"  {}:
    readable: [{}]
    updatable:
      status:
        only_when:
          - {{ old.status: open, new.status: [in_progress] }}
          - {{ old.status: in_progress, new.status: [open, resolved] }}
          - {{ old.status: resolved, new.status: [closed] }}
        guidance: "Follow the ticket lifecycle: open → in_progress → resolved → closed"
      priority:
        requires_approval: true
        guidance: "Priority changes require manager approval"
    deletable: false"#,
                        t.name,
                        columns.join(", ")
                    )
                } else {
                    // Other tables - read-only
                    format!(
                        r#"  {}:
    readable: [{}]
    # Read-only access for this table"#,
                        t.name,
                        columns.join(", ")
                    )
                }
            })
            .collect()
    };

    format!(
        r#"# =============================================================================
# SUPPORT AGENT ROLE
# =============================================================================
# Customer support agent with access to customer-facing data.
# Can view most data, limited write access to tickets/issues.
# Note: Tables not listed here are implicitly blocked.
# Conforms to: RoleDefinition.schema.json
# =============================================================================

name: support_agent
description: "AI agent for customer support operations"

# Default approval group for requires_approval flags
approvals:
  group: support_managers
  notify_on_pending: true
  message: "Support action requires manager approval"

tables:
{}
"#,
        table_entries.join("\n\n")
    )
}

/// Generate README content.
fn generate_readme(
    project: &str,
    tenant_column: &Option<String>,
    tenancy_analysis: &TenancyAnalysis,
) -> String {
    let tenant_col = tenant_column
        .clone()
        .unwrap_or_else(|| "organization_id".to_string());

    // Generate FK chain documentation if there are any
    let fk_tables: Vec<_> = tenancy_analysis
        .table_tenancy
        .iter()
        .filter_map(|(name, source)| {
            if let TenancySource::ViaForeignKey {
                fk_column, chain, ..
            } = source
            {
                let chain_desc: Vec<String> =
                    chain.iter().map(|(t, c)| format!("{}.{}", t, c)).collect();
                Some((name.clone(), fk_column.clone(), chain_desc.join(" → ")))
            } else {
                None
            }
        })
        .collect();

    let fk_section = if fk_tables.is_empty() {
        String::new()
    } else {
        let mut rows = String::from("### FK-Inherited Tenant Isolation\n\n");
        rows.push_str("Some tables don't have a direct tenant column but inherit tenant context via foreign key relationships.\n");
        rows.push_str("Cori will use JOINs to enforce tenant isolation on these tables:\n\n");
        rows.push_str("| Table | FK Column | Path to Tenant |\n");
        rows.push_str("|-------|-----------|----------------|\n");
        for (table, fk_col, chain) in &fk_tables {
            rows.push_str(&format!("| `{}` | `{}` | {} |\n", table, fk_col, chain));
        }
        rows.push_str("\n**Example:** When querying a table with FK-inherited tenancy:\n");
        rows.push_str("```sql\n");
        rows.push_str("-- Original query\n");
        rows.push_str(&format!(
            "SELECT * FROM {} WHERE status = 'active'\n",
            fk_tables
                .first()
                .map(|(t, _, _)| t.as_str())
                .unwrap_or("child_table")
        ));
        rows.push_str("\n-- Cori rewrites to (using JOIN for tenant isolation)\n");
        if let Some((table, fk_col, _)) = fk_tables.first() {
            let parent = tenancy_analysis
                .table_tenancy
                .get(table)
                .and_then(|s| {
                    if let TenancySource::ViaForeignKey { chain, .. } = s {
                        chain.first().map(|(t, c)| (t.clone(), c.clone()))
                    } else {
                        None
                    }
                })
                .unwrap_or_else(|| ("parent".to_string(), "id".to_string()));
            rows.push_str(&format!(
                "SELECT t.* FROM {} t\n  JOIN {} p ON t.{} = p.{}\n  WHERE t.status = 'active' AND p.{} = 'acme_corp'\n",
                table, parent.0, fk_col, parent.1, tenant_col
            ));
        }
        rows.push_str("```\n");
        rows
    };

    format!(
        r#"# {project}

A Cori MCP server project for secure, tenant-isolated database access.

## Overview

This project was initialized from an existing database schema using `cori init`.
Cori acts as a security layer between AI agents and your PostgreSQL database,
enforcing tenant isolation and role-based access control via the Model Context Protocol (MCP).

## Project Structure

```
{project}/
├── cori.yaml              # Main configuration
├── schema/                # Database schema files
│   ├── schema.yaml        # Auto-generated DB schema (DO NOT EDIT)
│   ├── rules.yaml         # Tenancy, soft-delete, validation rules
│   └── types.yaml         # Reusable semantic types for validation
├── roles/                 # Role definitions (one file per role)
│   ├── admin_agent.yaml
│   ├── readonly_agent.yaml
│   └── support_agent.yaml
├── groups/                # Approval groups for human-in-the-loop
│   └── support_managers.yaml
├── keys/                  # Biscuit keypair
│   ├── private.key        # ⚠️ Keep secret! Don't commit!
│   └── public.key
└── tokens/                # Generated tokens (gitignored)
```

## Quick Start

### 1. Configure Database Connection

Set the database URL via environment variable:

```bash
export DATABASE_URL="postgres://user:password@host:5432/database"
```

### 2. Review Configuration Files

**Schema rules** (`schema/rules.yaml`):
- Defines which column holds tenant IDs in each table
- Configures soft-delete behavior
- Sets up column validation and PII tagging

**Role definitions** (`roles/`):
- `admin_agent.yaml` - Full access (use sparingly)
- `readonly_agent.yaml` - Read-only analytics access  
- `support_agent.yaml` - Customer support operations

**Approval groups** (`groups/`):
- Define who can approve human-in-the-loop actions

### 3. Start the Server

```bash
cori run
```

This starts:
- MCP server (stdio mode for Claude Desktop, or HTTP)
- Admin dashboard on port **8080**

### 4. Mint Tokens

**Step 1: Create a role token** (base token without tenant restriction):

```bash
cori token mint --role support_agent --output tokens/support_role.token
```

**Step 2: Attenuate to a specific tenant** (when deploying to an AI agent):

```bash
cori token attenuate \
    --base tokens/support_role.token \
    --tenant acme_corp \
    --expires 24h \
    --output tokens/acme_support.token
```

**Or mint a tenant-specific token directly:**

```bash
cori token mint \
    --role support_agent \
    --tenant acme_corp \
    --expires 24h \
    --output tokens/acme_support.token
```

## Tenant Isolation

All queries are automatically scoped to the tenant specified in the token.

**Configured tenant column:** `{tenant_col}`

When an agent executes a tool:
```json
{{"tool": "listCustomers", "arguments": {{"status": "active"}}}}
```

Cori automatically adds tenant filtering:
```sql
SELECT * FROM customers WHERE status = 'active' AND {tenant_col} = 'acme_corp'
```

{fk_section}

## MCP (Model Context Protocol)

Cori exposes typed database actions as MCP tools for AI agents.

### Claude Desktop Configuration

Add to `claude_desktop_config.json`:

```json
{{
  "mcpServers": {{
    "cori": {{
      "command": "cori",
      "args": ["run", "--stdio"],
      "env": {{"CORI_TOKEN": "<base64 token>"}}
    }}
  }}
}}
```

### HTTP Transport

For agents that prefer HTTP, update `cori.yaml`:

```yaml
mcp:
  enabled: true
  transport: http
  host: 127.0.0.1
  port: 3000
```

## Commands Reference

| Command | Description |
|---------|-------------|
| `cori run` | Start the MCP server and dashboard |
| `cori run --stdio` | Start MCP server (stdio mode) |
| `cori db sync` | Update schema/schema.yaml from database |
| `cori keys generate` | Generate new Biscuit keypair |
| `cori token mint` | Mint a new role token |
| `cori token attenuate` | Attenuate a token to a tenant |
| `cori token inspect` | Inspect token claims |
| `cori token verify` | Verify token validity |

## Security Notes

1. **Never commit `keys/private.key`** - It's gitignored by default
2. **Use short token lifetimes** - Default is 24h, adjust as needed
3. **Review role permissions** - Generated roles are starting points
4. **Review `schema/rules.yaml`** - Especially tables with FK-inherited tenancy
5. **Enable TLS in production** - Use a reverse proxy like nginx or Caddy
6. **Update `groups/`** - Add actual team member emails for approvals

## Documentation

- [Cori Documentation](https://github.com/your-org/cori)
- [Biscuit Tokens](https://www.biscuitsec.org/)
- [MCP Specification](https://modelcontextprotocol.io/)
"#,
        project = project,
        tenant_col = tenant_col,
        fk_section = fk_section
    )
}

// -----------------------------------------------------------------------------
// Helper types
// -----------------------------------------------------------------------------

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct TableInfo {
    schema: String,
    name: String,
    columns: Vec<ColumnInfo>,
    primary_key: Vec<String>,
    foreign_keys: Vec<ForeignKeyInfo>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct ColumnInfo {
    name: String,
    data_type: String,
    nullable: bool,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct ForeignKeyInfo {
    name: String,
    column: String,
    references_table: String,
    references_column: String,
}

/// Describes how a table gets its tenant context
#[derive(Debug, Clone)]
#[allow(dead_code)]
enum TenancySource {
    /// Table has a direct tenant column
    Direct { column: String, data_type: String },
    /// Table inherits tenant via FK chain
    ViaForeignKey {
        /// FK column in this table
        fk_column: String,
        /// Chain of (table, column) pairs to reach tenant
        chain: Vec<(String, String)>,
        /// Final tenant column at the end of the chain
        tenant_column: String,
    },
    /// Table is global (no tenant isolation)
    Global,
    /// Cannot determine tenancy (needs manual config)
    Unknown,
}

/// Analysis result for tenancy configuration
#[derive(Debug)]
struct TenancyAnalysis {
    /// Detected tenant column name
    tenant_column: Option<String>,
    /// Per-table tenancy source
    table_tenancy: std::collections::HashMap<String, TenancySource>,
    /// Tables requiring attention (indirect FK, needs manual review)
    warnings: Vec<String>,
}
