use clap::{Parser, Subcommand};

use serde::Deserialize;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

// CLI commands module
mod commands;

#[derive(Parser, Debug)]
#[command(name = "cori", version, about = "Cori Store CLI - governed Postgres data for AI agents")]
struct Cli {
    #[command(subcommand)]
    cmd: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Initialize a Cori project from an existing database.
    ///
    /// This command introspects your database schema and creates a complete
    /// project structure with:
    /// - Configuration file (cori.yaml) with tenant isolation settings
    /// - Biscuit keypair for token authentication (keys/)
    /// - Sample role definitions (roles/)
    /// - Schema snapshot (schema/)
    /// - Proper .gitignore for security
    Init {
        /// Database URL (Postgres), e.g. postgres://user:pass@host:5432/db
        #[arg(long = "from-db")]
        from_db: String,

        /// Project name (also used as output directory)
        #[arg(long)]
        project: String,

        /// Overwrite if the project directory already exists
        #[arg(long, default_value_t = false)]
        force: bool,
    },

    /// Database schema management.
    Db {
        #[command(subcommand)]
        cmd: DbCommand,
    },

    /// Generate Biscuit keypair for token signing.
    Keys {
        #[command(subcommand)]
        cmd: KeysCommand,
    },

    /// Biscuit token management (mint, attenuate, inspect).
    Token {
        #[command(subcommand)]
        cmd: TokenCommand,
    },

    /// Start Cori (Dashboard + MCP server).
    ///
    /// By default, uses the transport setting from cori.yaml (mcp.transport).
    /// If not configured, defaults to HTTP mode on :3000.
    /// Dashboard runs on :8080 by default. Use --stdio for Claude Desktop.
    Run {
        /// Path to configuration file.
        #[arg(long, short, default_value = "cori.yaml")]
        config: PathBuf,

        /// Use HTTP transport (overrides config). Multi-tenant: each request carries its own token.
        #[arg(long, conflicts_with = "stdio")]
        http: bool,

        /// Use stdio transport (overrides config). Single-tenant: requires --token or CORI_TOKEN env.
        #[arg(long, conflicts_with = "http")]
        stdio: bool,

        /// Token file (required with --stdio unless CORI_TOKEN env is set).
        #[arg(long, short)]
        token: Option<PathBuf>,

        /// MCP HTTP port (only with --http). Default: 3000 or from config.
        #[arg(long)]
        mcp_port: Option<u16>,

        /// Dashboard port. Default: 8080 or from config.
        #[arg(long)]
        dashboard_port: Option<u16>,

        /// Disable dashboard (MCP only).
        #[arg(long)]
        no_dashboard: bool,
    },

    /// Tool introspection (offline, no server needed).
    Tools {
        #[command(subcommand)]
        cmd: ToolsCommand,
    },

    /// Validate configuration files for consistency and correctness.
    ///
    /// This command performs comprehensive validation:
    /// - JSON Schema validation against schemas in schemas/
    /// - Cross-file consistency checks (tables, columns, groups)
    /// - Best practice warnings (soft delete, approval groups)
    Check {
        /// Path to configuration file (YAML).
        #[arg(long, short, default_value = "cori.yaml")]
        config: PathBuf,
    },
}

#[derive(Subcommand, Debug)]
enum KeysCommand {
    /// Generate a new Ed25519 keypair for Biscuit token signing.
    Generate {
        /// Output directory for key files. If not specified, prints to stdout.
        #[arg(long, short)]
        output: Option<PathBuf>,
    },
}

#[derive(Subcommand, Debug)]
enum TokenCommand {
    /// Mint a new role token (or agent token if --tenant is specified).
    Mint {
        /// Path to configuration file (for key resolution).
        #[arg(long, short, default_value = "cori.yaml")]
        config: PathBuf,

        /// Path to private key file (overrides cori.yaml). Falls back to BISCUIT_PRIVATE_KEY env var.
        #[arg(long, env = "BISCUIT_PRIVATE_KEY")]
        key: Option<String>,

        /// Role name for the token.
        #[arg(long)]
        role: String,

        /// Tenant ID (if specified, creates an attenuated agent token).
        #[arg(long)]
        tenant: Option<String>,

        /// Expiration duration (e.g., "24h", "7d", "1h").
        #[arg(long)]
        expires: Option<String>,

        /// Tables to grant access to (format: "table:col1,col2" or just "table").
        #[arg(long = "table")]
        tables: Vec<String>,

        /// Output file path. If not specified, prints to stdout.
        #[arg(long, short)]
        output: Option<PathBuf>,
    },

    /// Attenuate a role token with tenant restriction and expiration.
    Attenuate {
        /// Path to configuration file (for key resolution).
        #[arg(long, short, default_value = "cori.yaml")]
        config: PathBuf,

        /// Path to private key file (overrides cori.yaml). Falls back to BISCUIT_PRIVATE_KEY env var.
        #[arg(long, env = "BISCUIT_PRIVATE_KEY")]
        key: Option<String>,

        /// Path to base role token file.
        #[arg(long)]
        base: PathBuf,

        /// Tenant ID to restrict the token to.
        #[arg(long)]
        tenant: String,

        /// Expiration duration (e.g., "24h", "7d").
        #[arg(long)]
        expires: Option<String>,

        /// Output file path. If not specified, prints to stdout.
        #[arg(long, short)]
        output: Option<PathBuf>,
    },

    /// Inspect a token's contents (optionally verify signature).
    ///
    /// Without --verify: shows token contents with "signature not verified" warning.
    /// With --verify: verifies signature using key from cori.yaml or --key.
    Inspect {
        /// Token string or path to token file.
        token: String,

        /// Path to configuration file (for key resolution).
        #[arg(long, short, default_value = "cori.yaml")]
        config: PathBuf,

        /// Path to public key file (overrides cori.yaml). Falls back to BISCUIT_PUBLIC_KEY env var.
        #[arg(long, env = "BISCUIT_PUBLIC_KEY")]
        key: Option<String>,

        /// Verify the token signature using the public key from config or --key.
        #[arg(long)]
        verify: bool,
    },
}

#[derive(Subcommand, Debug)]
enum ToolsCommand {
    /// List tools for a role or token (offline, no server needed).
    List {
        /// Path to configuration file (YAML).
        #[arg(long, short, default_value = "cori.yaml")]
        config: PathBuf,

        /// Role name to generate tools for.
        #[arg(long, conflicts_with = "token")]
        role: Option<String>,

        /// Token file to extract role from.
        #[arg(long, short, conflicts_with = "role")]
        token: Option<PathBuf>,

        /// Show detailed tool schemas.
        #[arg(long)]
        verbose: bool,
    },

    /// Show detailed schema for a specific tool.
    Describe {
        /// Path to configuration file (YAML).
        #[arg(long, short, default_value = "cori.yaml")]
        config: PathBuf,

        /// Tool name.
        tool: String,

        /// Role name.
        #[arg(long)]
        role: String,
    },
}

#[derive(Subcommand, Debug)]
enum DbCommand {
    /// Sync database schema to schema/schema.yaml.
    ///
    /// Introspects the configured database and generates a YAML schema definition
    /// that can be used for role-based access control configuration.
    Sync {
        /// Path to configuration file.
        #[arg(long, short, default_value = "cori.yaml")]
        config: PathBuf,
    },
}

/// Run configuration check as a pre-hook with a custom config path.
async fn run_pre_hook_check_with_config(config_path: &Path) -> anyhow::Result<()> {
    commands::check::run_pre_hook(config_path).await
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Configure tracing to write to stderr (not stdout) to avoid contaminating
    // JSON-RPC messages when using stdio transport for MCP
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false) // Disable ANSI colors to avoid escape codes
        .with_env_filter("info")
        .init();

    let cli = Cli::parse();

    match cli.cmd {
        Command::Init {
            from_db,
            project,
            force,
        } => commands::init::run(&from_db, &project, force).await?,

        Command::Db { cmd } => {
            // Note: We intentionally skip pre-hook check for db sync command
            // because the purpose of db sync is to regenerate schema.yaml,
            // which might be missing or in an old format.
            match cmd {
                DbCommand::Sync { config } => run_db_sync(&config).await?,
            }
        }

        // ===== Phase 1: Core Command Handlers =====
        Command::Keys { cmd } => match cmd {
            KeysCommand::Generate { output } => {
                commands::keys::generate(output)?;
            }
        },

        Command::Token { cmd } => match cmd {
            TokenCommand::Mint {
                config,
                key,
                role,
                tenant,
                expires,
                tables,
                output,
            } => {
                commands::token::mint(config, key, role, tenant, expires, tables, output)?;
            }
            TokenCommand::Attenuate {
                config,
                key,
                base,
                tenant,
                expires,
                output,
            } => {
                commands::token::attenuate(config, key, base, tenant, expires, output)?;
            }
            TokenCommand::Inspect { token, config, key, verify } => {
                commands::token::inspect(config, token, key, verify)?;
            }
        },

        Command::Run {
            config,
            http,
            stdio,
            token,
            mcp_port,
            dashboard_port,
            no_dashboard,
        } => {
            run_pre_hook_check_with_config(&config).await?;
            commands::run::run(
                config,
                http,
                stdio,
                token,
                mcp_port,
                dashboard_port,
                no_dashboard,
            )
            .await?;
        }

        Command::Tools { cmd } => match cmd {
            ToolsCommand::List {
                config,
                role,
                token,
                verbose,
            } => {
                commands::tools::list(config, role, token, verbose)?;
            }
            ToolsCommand::Describe { config, tool, role } => {
                commands::tools::describe(config, tool, role)?;
            }
        },

        Command::Check { config } => {
            commands::check::run(&config).await?;
        }
    }

    Ok(())
}

// -----------------------------
// db commands
// -----------------------------

/// Upstream database configuration from cori.yaml.
#[derive(Debug, Deserialize)]
struct UpstreamConfig {
    /// Hostname (optional if database_url_env is set)
    host: Option<String>,
    #[serde(default = "default_upstream_port")]
    port: u16,
    /// Database name (optional if database_url_env is set)
    database: Option<String>,
    /// Username (optional if database_url_env is set)
    username: Option<String>,
    password: Option<String>,
    /// Environment variable containing DATABASE_URL (recommended)
    database_url_env: Option<String>,
    /// Direct database URL (for development only)
    database_url: Option<String>,
}

fn default_upstream_port() -> u16 {
    5432
}

/// Configuration file structure for db commands.
#[derive(Debug, Deserialize)]
struct DbConfig {
    upstream: UpstreamConfig,
}

/// Build database URL from upstream config.
fn build_database_url(upstream: &UpstreamConfig) -> anyhow::Result<String> {
    // First check for database_url_env
    if let Some(env_var) = &upstream.database_url_env
        && let Ok(url) = env::var(env_var) {
            return Ok(url);
        }

    // Check for direct database_url
    if let Some(url) = &upstream.database_url {
        return Ok(url.clone());
    }

    // Also check DATABASE_URL directly
    if let Ok(url) = env::var("DATABASE_URL") {
        return Ok(url);
    }

    // Build from individual components
    let host = upstream.host.as_deref().unwrap_or("localhost");
    let port = upstream.port;
    let database = upstream.database.as_deref().ok_or_else(|| {
        anyhow::anyhow!("Database name required in upstream config or DATABASE_URL env var")
    })?;
    let username = upstream.username.as_deref().unwrap_or("postgres");
    let password = upstream.password.as_deref().unwrap_or("");

    Ok(format!(
        "postgres://{}:{}@{}:{}/{}",
        username, password, host, port, database
    ))
}

async fn run_db_sync(config_path: &Path) -> anyhow::Result<()> {
    if !config_path.exists() {
        return Err(anyhow::anyhow!(
            "Configuration file not found: {}. Run this inside a Cori project.",
            config_path.display()
        ));
    }

    let contents = fs::read_to_string(config_path)?;
    let cfg: DbConfig = serde_yaml::from_str(&contents)?;
    let db_url = build_database_url(&cfg.upstream)?;

    // Determine schema directory relative to config file
    let config_dir = config_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let schema_dir = config_dir.join("schema");

    fs::create_dir_all(&schema_dir)?;
    let schema_path = schema_dir.join("schema.yaml");

    // Introspect database schema
    let snapshot = cori_adapter_pg::introspect::introspect_schema_json(&db_url).await?;

    // Convert to YAML format
    let yaml = serde_yaml::to_string(&snapshot)?;
    fs::write(&schema_path, yaml)?;
    println!("✔ Wrote schema definition: {}", schema_path.display());

    Ok(())
}
