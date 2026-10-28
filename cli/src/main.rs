use anyhow::{Context, Result};
use chrono::{Duration, Utc};
use clap::{Parser, Subcommand, ValueEnum};
use rustyline::DefaultEditor;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use varynth::automation::{parse_trigger_with_cron, TaskStore, Trigger};
use varynth::config::{
    keyring_delete, keyring_get, keyring_set, normalize_keyring_field, profile_keyring_delete,
    profile_keyring_set, Config, KEYRING_FIELDS,
};
use varynth::runtime::Runtime;
use varynth::session::Session;
use varynth::{
    accounts, channels, dashboard, doctor, google_auth, install, mcp, plugins, skills_search, tui,
    varynth_auth,
};

#[derive(Parser, Debug)]
#[command(
    name = "varynth",
    version,
    about = "Varynth local coding workspace with a TUI, secure tools, MCP, goals and an operator dashboard",
    styles = varynth::theme::CLAP_STYLES
)]
struct Cli {
    /// Working directory
    #[arg(short = 'C', long)]
    cwd: Option<PathBuf>,
    /// Model id (provider:name on proxy)
    #[arg(short, long)]
    model: Option<String>,
    /// Provider: proxy | anthropic | openai | google | google-vertex, or a configured profile id
    #[arg(long)]
    provider: Option<String>,
    /// Permission mode: acceptEdits | prompt | bypass | plan
    #[arg(long)]
    permission_mode: Option<String>,
    /// Sandbox: workspace-write | read-only | danger-full-access | docker-isolated (or docker)
    #[arg(long)]
    sandbox: Option<String>,
    /// Disable the full-screen TUI (plain readline)
    #[arg(long)]
    plain: bool,
    /// JSON file containing MCP servers under the `mcpServers` key
    #[arg(long, value_name = "FILE")]
    mcp_config: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Non-interactive one-shot: run a prompt and print the reply
    Exec {
        prompt: String,
        /// text (human), json (single result object), or stream-json (JSONL event envelopes)
        #[arg(long, value_enum, default_value_t = OutputMode::Text)]
        output: OutputMode,
    },
    /// Resume the latest session, or a specific id
    Resume { id: Option<String> },
    /// List saved sessions, or fork/export one
    Sessions {
        #[command(subcommand)]
        command: Option<SessionsCommand>,
    },
    /// Authentication management (Google OAuth, Antigravity, or Varynth Cloud)
    Auth {
        #[command(subcommand)]
        command: Option<AuthCommand>,
    },
    /// Google OAuth login, status, or logout
    Google {
        #[command(subcommand)]
        command: GoogleCommand,
    },
    /// List models from the active provider or all configured profiles
    Models,
    /// Health / auth / proxy checks
    Doctor {
        /// Start local 8787 proxy if it is down
        #[arg(long)]
        fix: bool,
    },
    /// Read and write secrets in the OS keyring
    Keyring {
        #[command(subcommand)]
        command: KeyringCommand,
    },
    /// Manage the curated local plugin marketplace
    Plugin { spec: Vec<String> },
    /// Manage installed/searchable skills
    Skill {
        #[command(subcommand)]
        command: SkillCommand,
    },
    /// Inspect or explicitly probe configured MCP servers
    Mcp {
        #[command(subcommand)]
        command: McpCommand,
    },
    /// Copy this binary to ~/.local/bin
    Install {
        /// Register a Windows logon task that runs `varynth serve`
        #[arg(long)]
        startup: bool,
    },
    /// Check for a newer release and upgrade in place
    Update {
        /// Only report whether a newer version exists
        #[arg(long)]
        check: bool,
        /// Installer to use: cargo | npm (auto-detected by default)
        #[arg(long, value_name = "METHOD")]
        via: Option<String>,
    },
    /// Local web deck and channel gateway
    Serve {
        #[arg(long)]
        host: Option<String>,
        #[arg(long)]
        port: Option<u16>,
    },
    /// Interactive REPL (default)
    Repl,
    /// Write default ~/.varynth/config.toml
    Init,
    /// Channel status (Telegram live with serve; Discord/WhatsApp planned)
    Channels,
    /// Desktop notifications: status, or fire one test toast
    Notify {
        #[command(subcommand)]
        command: NotifyCommand,
    },
    /// Manage model-driven reminders and automation tasks
    Task {
        #[command(subcommand)]
        command: TaskCommand,
    },
    /// Link one official account. Replaces any previous secret for that account.
    Account {
        #[command(subcommand)]
        command: AccountCommand,
    },
    /// Sign in to a Varynth account in the browser.
    Login {
        /// Print the URL without opening a browser.
        #[arg(long)]
        no_browser: bool,
    },
    /// Remove the local Varynth account session.
    Logout,
    /// Trust this workspace's project-scoped permissions (or revoke trust).
    Trust {
        /// Remove the trust decision instead of adding it
        #[arg(long)]
        revoke: bool,
    },
    /// List user-defined slash commands (user + trusted project)
    Commands {
        #[command(subcommand)]
        command: Option<CommandsCommand>,
    },
    /// Ask the local Laya/Clef decision engines for typed, probabilistic answers.
    Decide {
        /// Full request JSON (`{"state":..., "questions":{...}}`).
        #[arg(long, conflicts_with = "file")]
        json: Option<String>,
        /// File containing the full request JSON.
        #[arg(long, value_name = "FILE")]
        file: Option<PathBuf>,
        /// Inline state: parsed as JSON when it parses, otherwise used as text.
        #[arg(long, required_unless_present_any = ["json", "file"])]
        state: Option<String>,
        /// Inline questions object.
        #[arg(long, required_unless_present_any = ["json", "file"])]
        questions: Option<String>,
        /// Which engine answers: auto routes Laya first, then Clef.
        #[arg(long, value_parser = ["auto", "laya", "clef"])]
        engine: Option<String>,
    },
    /// Add or remove one OpenAI-compatible or Anthropic-compatible endpoint.
    Endpoint {
        #[command(subcommand)]
        command: EndpointCommand,
    },
    /// Local Laya/Clef service: status, start, stop.
    Decisions {
        #[command(subcommand)]
        command: DecisionsCommand,
    },
    /// Read or persist non-secret settings, or open config.toml in $EDITOR.
    #[command(next_help_heading = "Configure")]
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// List the built-in tool schemas the model can call.
    Tools {
        /// Print a JSON array of {name, description}.
        #[arg(long)]
        json: bool,
    },
    /// List, set, and save terminal themes.
    #[command(next_help_heading = "Configure")]
    Theme {
        #[command(subcommand)]
        command: ThemeCommand,
    },
    /// Desktop control status and the open-computer-use adapter.
    Computer {
        #[command(subcommand)]
        command: ComputerCommand,
    },
    /// Run an automated Git diff & PR security/bug review using the Advisor role.
    Review {
        /// Review only staged changes (`git diff --staged`).
        #[arg(long)]
        staged: bool,
    },
    /// Execute a 3-stage Multi-Agent Swarm (Architect Plan -> Builder Default -> Auditor Advisor).
    Swarm {
        /// The engineering task to orchestrate across the 3-agent swarm.
        task: String,
    },
    /// Sync local CLI session telemetry to the Varynth Cloud Dashboard (`/v1/me/sessions`).
    Sync,
}

#[derive(Subcommand, Debug)]
enum ConfigCommand {
    /// Print one non-secret setting, or every allowlisted setting.
    Get {
        key: Option<String>,
        /// Print one JSON object instead of key=value lines.
        #[arg(long)]
        json: bool,
    },
    /// Validate and persist one non-secret setting.
    Set { key: String, value: String },
    /// Open config.toml in $VISUAL or $EDITOR.
    Edit,
}

#[derive(Subcommand, Debug)]
enum DecisionsCommand {
    /// Reachability, weights, autostart, resolved service path, managed pid.
    Status,
    /// Start the local service if needed and wait for /health.
    Start,
    /// Stop the service, but only when this CLI started it.
    Stop,
}

#[derive(Subcommand, Debug)]
enum EndpointCommand {
    /// Store one compatible endpoint. Secret is optional; `-` reads stdin.
    Add {
        id: String,
        #[arg(long, value_parser = ["openai", "anthropic"])]
        kind: String,
        #[arg(long)]
        base_url: String,
        secret: Option<String>,
    },
    /// List saved endpoint ids and kinds. Never prints secrets.
    List,
    /// Remove one saved endpoint profile and its keyring secret.
    Remove { id: String },
}

#[derive(Subcommand, Debug)]
#[command(arg_required_else_help = true)]
enum ThemeCommand {
    /// Print built-in and custom theme names.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Print one theme's roles. Defaults to the saved theme.
    Get {
        name: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Save a built-in or custom:<name> theme.
    Set {
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// Paint a one-second swatch. Skipped when stdout is not a TTY.
    Preview { name: String },
    /// Save or locate a custom theme file.
    #[command(subcommand)]
    Custom(ThemeCustomCommand),
}

#[derive(Subcommand, Debug)]
#[command(arg_required_else_help = true)]
enum ThemeCustomCommand {
    /// Read a role JSON object from stdin and write themes/<name>.json.
    Save { name: String },
    /// Print the path of one custom theme file.
    Path { name: String },
}

#[derive(Subcommand, Debug)]
#[command(arg_required_else_help = true)]
enum ComputerCommand {
    /// Show whether desktop control is enabled and which backend is active.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Report the desktop backend. Exit 0 when the report runs.
    Doctor,
    /// Save computer_use and set VARYNTH_COMPUTER_USE=1 for this process.
    Enable,
    /// Save computer_use off and clear VARYNTH_COMPUTER_USE in this process.
    Disable,
    /// Write the open-computer-use stdio MCP entry.
    Install {
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand, Debug)]
enum AccountCommand {
    /// Show which single-account slots are present. Never prints secrets.
    Status,
    /// Store one key and, for model accounts, the official endpoint.
    Connect {
        provider: String,
        /// Secret, or `-` to read stdin. Omit only to keep the stored secret.
        secret: Option<String>,
        /// Cloudflare account id (32 hex). Workers uses the same slot.
        #[arg(long)]
        account_id: Option<String>,
    },
    /// Remove the single stored account and its provider profile.
    Disconnect { provider: String },
}

#[derive(Subcommand, Debug)]
enum KeyringCommand {
    /// Set a secret. Omit value for masked terminal input; use '-' for stdin.
    Set {
        field: String,
        value: Option<String>,
    },
    /// Print one secret with an explicit warning.
    Get { field: String },
    /// Delete one secret.
    Delete { field: String },
    /// Print field presence only, never secret values.
    Status,
}

#[derive(Subcommand, Debug)]
enum GoogleCommand {
    Login {
        #[arg(long, default_value = google_auth::AG_CLIENT_ID)]
        client_id: String,
        #[arg(long)]
        client_secret: Option<String>,
        #[arg(long)]
        no_browser: bool,
    },
    Status,
    Logout,
}

#[derive(Subcommand, Debug)]
enum AuthCommand {
    /// Google OAuth login / status / logout
    Google {
        #[command(subcommand)]
        command: Option<GoogleCommand>,
        #[arg(long, default_value = google_auth::AG_CLIENT_ID)]
        client_id: String,
        #[arg(long)]
        client_secret: Option<String>,
        #[arg(long)]
        no_browser: bool,
    },
    /// Google Antigravity OAuth login / sync
    #[command(alias = "ag")]
    Antigravity {
        #[command(subcommand)]
        command: Option<GoogleCommand>,
        #[arg(long, default_value = google_auth::AG_CLIENT_ID)]
        client_id: String,
        #[arg(long)]
        client_secret: Option<String>,
        #[arg(long)]
        no_browser: bool,
    },
    /// Varynth Cloud sign-in
    #[command(alias = "cloud")]
    Varynth {
        #[arg(long)]
        no_browser: bool,
    },
    /// Browser-based sign-in to Varynth Cloud
    Login {
        #[arg(long)]
        no_browser: bool,
    },
    /// Remove authentication tokens
    Logout {
        #[arg(long)]
        all: bool,
    },
    /// Print active authentication status for all providers
    Status,
}

#[derive(Subcommand, Debug)]
enum SkillCommand {
    List,
    Search {
        query: String,
    },
    Install {
        source: String,
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand, Debug)]
enum CommandsCommand {
    /// List discovered commands (the default)
    List {
        /// Print a JSON array instead of one line per command
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
enum McpCommand {
    Status,
    Probe {
        name: String,
    },
    Ping {
        name: String,
    },
    /// Speak MCP on stdio. `web-search` calls the signed-in Varynth API.
    Serve {
        name: String,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum, PartialEq, Eq)]
enum OutputMode {
    Text,
    Json,
    StreamJson,
}

#[derive(Subcommand, Debug)]
enum SessionsCommand {
    /// Branch a session at a message count (default: all messages)
    Fork {
        id: String,
        /// Keep only the first N messages; the dropped tail is summarized
        #[arg(long)]
        at: Option<usize>,
    },
    /// Write a redacted Markdown or JSON transcript
    Export {
        id: String,
        /// md or json
        #[arg(long, default_value = "md", value_parser = ["md", "json"])]
        format: String,
        /// Output path (default: ./varynth-session-<id>.<ext>)
        #[arg(long)]
        out: Option<PathBuf>,
        /// Overwrite an existing output file
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand, Debug)]
enum NotifyCommand {
    /// Show the configured mode and OS notification mechanism
    Status,
    /// Fire one OS notification now, bypassing mode, focus and coalescing
    Test {
        #[arg(default_value = "Notification test")]
        label: String,
    },
}

#[derive(Subcommand, Debug)]
enum TaskCommand {
    Add {
        name: String,
        prompt: String,
        #[arg(long)]
        at: Option<String>,
        #[arg(long)]
        every_seconds: Option<u64>,
        /// Five-field cron (minute hour day-of-month month day-of-week). A seconds field is prepended.
        #[arg(long)]
        cron: Option<String>,
        #[arg(long)]
        allow_background_tools: bool,
    },
    List,
    Run {
        id: String,
    },
    Disable {
        id: String,
    },
    Enable {
        id: String,
    },
    Remove {
        id: String,
    },
}

/// Diagnostics go to stderr, except in an interactive TUI run where a warning
/// printed mid-draw corrupts the alternate screen. There they go to
/// `~/.varynth/logs/varynth.log` instead (tail it to see the same lines).
fn init_logging(cli: &Cli) {
    let filter =
        std::env::var("RUST_LOG").unwrap_or_else(|_| "varynth=warn,tower_http=warn".into());
    let interactive = io::stdout().is_terminal()
        && matches!(
            cli.command.as_ref().unwrap_or(&Commands::Repl),
            Commands::Repl | Commands::Resume { .. }
        );
    if interactive {
        let dir = Config::home_dir().join("logs");
        let opened = std::fs::create_dir_all(&dir)
            .and_then(|_| {
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(dir.join("varynth.log"))
            })
            .map(std::sync::Mutex::new);
        if let Ok(writer) = opened {
            let _ = tracing_subscriber::fmt()
                .with_writer(writer)
                .with_ansi(false)
                .with_env_filter(filter)
                .with_target(false)
                .try_init();
            return;
        }
    }
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_logging(&cli);
    Config::ensure_home()?;
    let mut cfg = Config::load()?;
    // CLI flags and env selection overrides are one-shot. The bootstrap save
    // below must not turn them into persisted defaults.
    let model_overridden = cli.model.is_some()
        || std::env::var_os("VARYNTH_MODEL").is_some_and(|value| !value.is_empty());
    let provider_overridden = cli.provider.is_some()
        || ["VARYNTH_PROVIDER", "VARYNTH_PROFILE"]
            .iter()
            .any(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()));
    let mcp_explicit = cli.mcp_config.is_some()
        || std::env::var_os("VARYNTH_MCP_CONFIG").is_some_and(|value| !value.is_empty());
    if cfg.computer_use && std::env::var("VARYNTH_COMPUTER_USE").is_err() {
        std::env::set_var("VARYNTH_COMPUTER_USE", "1");
    }
    if let Some(m) = cli.model {
        cfg.select_model(&m)?;
    }
    if let Some(p) = cli.provider {
        cfg.provider = p;
    }
    if let Some(p) = cli.permission_mode {
        cfg.permission_mode = p;
    }
    if let Some(s) = cli.sandbox {
        cfg.sandbox = normalize_sandbox(&s)?;
    }
    if let Some(path) = cli.mcp_config {
        cfg.mcp_config = Some(path);
    }
    let cwd = cli
        .cwd
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let cwd = if cwd.is_absolute() {
        cwd
    } else {
        std::env::current_dir()?.join(cwd)
    };
    let trusted = varynth::trust::is_trusted(&cwd);
    let (selected_mcp, noticed) =
        select_mcp_config(&cwd, cfg.mcp_config.take(), mcp_explicit, trusted);
    cfg.mcp_config = selected_mcp;
    if let Some(message) = noticed {
        eprintln!("{message}");
    }
    cfg.validate()?;
    varynth::update::cleanup_old_binaries();
    // Release notice for human-facing entry points. Machine interfaces (exec
    // JSON, MCP stdio, serve) stay silent. The TUI shows it in its header
    // because stderr output would be wiped by the alternate screen.
    let command_ref = cli.command.as_ref().unwrap_or(&Commands::Repl);
    if matches!(
        command_ref,
        Commands::Repl | Commands::Resume { .. } | Commands::Models | Commands::Doctor { .. }
    ) && io::stderr().is_terminal()
    {
        if varynth::update::check_on_startup().await.is_some() {
            let tui_mode = matches!(command_ref, Commands::Repl | Commands::Resume { .. })
                && !cli.plain
                && io::stdout().is_terminal();
            if !tui_mode {
                if let Some(notice) = varynth::update::notice() {
                    eprintln!("{notice}");
                }
            }
        }
    }
    let uses_model = matches!(
        cli.command.as_ref().unwrap_or(&Commands::Repl),
        Commands::Exec { .. }
            | Commands::Review { .. }
            | Commands::Swarm { .. }
            | Commands::Models
            | Commands::Resume { .. }
            | Commands::Repl
            | Commands::Serve { .. }
            | Commands::Task {
                command: TaskCommand::Run { .. }
            }
    );
    if uses_model {
        google_auth::preflight(&cfg, false).await?;
        if let Err(err) = varynth::varynth_catalog::sync(&mut cfg).await {
            // The hosted catalog only matters when the Varynth door is the
            // active provider; a custom profile works fine while the local
            // API is down, so do not print a scary provider warning.
            if cfg.provider == "varynth" || cfg.model.starts_with("varynth/") {
                eprintln!("!! catalog unavailable\n   {err}\n   try: varynth doctor");
            }
        }
        cfg.ensure_public_door();
        // Persist only catalog-owned fields. A CLI override, an env secret or
        // an MCP path is never written back to config.toml.
        if let Err(err) = cfg.save_catalog(!model_overridden && !provider_overridden) {
            eprintln!("!! config save failed\n   {err}");
        }
    }

    match cli.command.unwrap_or(Commands::Repl) {
        Commands::Init => {
            cfg.save()?;
            println!("wrote {}", Config::config_path().display());
        }
        Commands::Trust { revoke } => {
            let workspace = cwd.clone();
            if revoke {
                if varynth::trust::revoke(&workspace)? {
                    println!("revoked trust for {}", workspace.display());
                } else {
                    println!("not trusted: {}", workspace.display());
                }
            } else {
                let entry = varynth::trust::trust(&workspace)?;
                println!("trusted {}  Â·  {}", entry.path, entry.trusted_at);
            }
        }
        Commands::Commands { command } => {
            print_commands(
                &cwd,
                command.unwrap_or(CommandsCommand::List { json: false }),
            );
        }
        Commands::Doctor { fix } => {
            if fix {
                match doctor::try_start_proxy(&cfg).await {
                    Ok(true) => eprintln!("proxy ok"),
                    Ok(false) => eprintln!("proxy started but /health did not come up"),
                    Err(e) => eprintln!("proxy fix: {e}"),
                }
            }
            let report = doctor::run(&cfg).await?;
            println!("sandbox backend: {}", doctor::sandbox_backend(&cfg.sandbox));
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        Commands::Account { command } => handle_account(command, &mut cfg)?,
        Commands::Keyring { command } => handle_keyring(command)?,
        Commands::Google { command } => match command {
            GoogleCommand::Login {
                client_id,
                client_secret,
                no_browser,
            } => {
                let secret = client_secret.as_deref().or_else(|| {
                    if client_id == google_auth::AG_CLIENT_ID {
                        Some(google_auth::AG_CLIENT_SECRET)
                    } else {
                        None
                    }
                });
                google_auth::login(&client_id, secret, no_browser).await?;
            }
            GoogleCommand::Status => {
                println!("{}", serde_json::to_string_pretty(&google_auth::status()?)?)
            }
            GoogleCommand::Logout => google_auth::logout()?,
        },
        Commands::Auth { command } => match command {
            Some(AuthCommand::Google { command, client_id, client_secret, no_browser }) => {
                match command {
                    Some(GoogleCommand::Login { client_id: cid, client_secret: csec, no_browser: nb }) => {
                        let sec = csec.as_deref().or_else(|| {
                            if cid == google_auth::AG_CLIENT_ID { Some(google_auth::AG_CLIENT_SECRET) } else { None }
                        });
                        google_auth::login(&cid, sec, nb).await?;
                    }
                    Some(GoogleCommand::Status) => {
                        println!("{}", serde_json::to_string_pretty(&google_auth::status()?)?);
                    }
                    Some(GoogleCommand::Logout) => {
                        google_auth::logout()?;
                        println!("Google OAuth signed out.");
                    }
                    None => {
                        let sec = client_secret.as_deref().or_else(|| {
                            if client_id == google_auth::AG_CLIENT_ID { Some(google_auth::AG_CLIENT_SECRET) } else { None }
                        });
                        google_auth::login(&client_id, sec, no_browser).await?;
                    }
                }
            }
            Some(AuthCommand::Antigravity { command, client_id, client_secret, no_browser }) => {
                match command {
                    Some(GoogleCommand::Login { client_id: cid, client_secret: csec, no_browser: nb }) => {
                        let sec = csec.as_deref().or_else(|| {
                            if cid == google_auth::AG_CLIENT_ID { Some(google_auth::AG_CLIENT_SECRET) } else { None }
                        });
                        google_auth::login(&cid, sec, nb).await?;
                    }
                    Some(GoogleCommand::Status) => {
                        println!("Antigravity Credentials Status:");
                        match google_auth::load_antigravity_credentials() {
                            Ok(info) => println!("  Active refresh token detected (project: {:?})", info.project_id),
                            Err(e) => println!("  No active Antigravity credentials: {e}"),
                        }
                    }
                    Some(GoogleCommand::Logout) => {
                        google_auth::logout()?;
                        println!("Antigravity credentials signed out.");
                    }
                    None => {
                        let sec = client_secret.as_deref().or_else(|| {
                            if client_id == google_auth::AG_CLIENT_ID { Some(google_auth::AG_CLIENT_SECRET) } else { None }
                        });
                        google_auth::login(&client_id, sec, no_browser).await?;
                    }
                }
            }
            Some(AuthCommand::Varynth { no_browser }) | Some(AuthCommand::Login { no_browser }) => {
                varynth_auth::login(&cfg.varynth_api_url, &cfg.varynth_web_url, no_browser).await?;
            }
            Some(AuthCommand::Logout { all }) => {
                varynth_auth::logout(&cfg.varynth_api_url).await?;
                if all {
                    let _ = google_auth::logout();
                    println!("Signed out of Varynth Cloud and Google OAuth.");
                } else {
                    println!("Signed out of Varynth Cloud.");
                }
            }
            Some(AuthCommand::Status) | None => {
                println!("--- Varynth Authentication Status ---");
                match varynth_auth::load_auth_env() {
                    Ok(Some(tok)) => {
                        let exp = tok.expires_at;
                        println!("  Varynth Cloud: Authenticated (token expires: {exp})");
                    }
                    _ => println!("  Varynth Cloud: Not signed in (run `varynth auth varynth` or `varynth login`)"),
                }
                match google_auth::status() {
                    Ok(st) => {
                        let is_auth = st.get("authenticated").and_then(|v| v.as_bool()).unwrap_or(false);
                        if is_auth {
                            println!("  Google OAuth:  Authenticated");
                        } else {
                            println!("  Google OAuth:  Not signed in (run `varynth auth google`)");
                        }
                    }
                    Err(_) => println!("  Google OAuth:  Not signed in"),
                }
                match google_auth::load_antigravity_credentials() {
                    Ok(ag) => println!("  Antigravity:   Active token detected (project: {:?})", ag.project_id),
                    Err(_) => println!("  Antigravity:   No local credentials file found"),
                }
                if command.is_none() {
                    println!("\nCommands:");
                    println!("  varynth auth google       - Sign in with Google");
                    println!("  varynth auth antigravity  - Sign in with Google Antigravity");
                    println!("  varynth auth varynth      - Sign in with Varynth Cloud");
                    println!("  varynth auth status       - Show authentication status");
                    println!("  varynth auth logout       - Sign out");
                }
            }
        },
        Commands::Plugin { spec } => {
            let spec = if spec.is_empty() {
                "list".into()
            } else {
                spec.join(" ")
            };
            let cwd = cwd.clone();
            let result = tokio::task::spawn_blocking(move || plugins::handle_command(&spec, &cwd))
                .await
                .context("plugin command task failed")??;
            println!("{result}");
        }
        Commands::Skill { command } => handle_skill(command).await?,
        Commands::Mcp { command } => handle_mcp(command, cfg.mcp_config.as_deref())?,
        Commands::Install { startup } => {
            install::run(startup)?;
        }
        Commands::Update { check, via } => {
            varynth::update::run(check, via).await?;
        }
        Commands::Models => {
            let rt = Runtime::new(cfg, cwd)?;
            let models = rt.list_models().await?;
            for m in models {
                println!("{}", m.id);
            }
        }
        Commands::Sessions { command } => match command {
            None => {
                for s in Session::list()? {
                    println!(
                        "{}  {}  {}{}",
                        s.id,
                        s.updated_at.format("%Y-%m-%d %H:%M"),
                        s.title,
                        s.parent
                            .as_deref()
                            .map(|parent| format!("  (fork of {parent})"))
                            .unwrap_or_default()
                    );
                }
            }
            Some(SessionsCommand::Fork { id, at }) => {
                let fork = Session::fork_from(&id, at)?;
                println!("{}", fork.meta.id);
            }
            Some(SessionsCommand::Export {
                id,
                format,
                out,
                force,
            }) => {
                let session = Session::load(&id)?;
                let format = varynth::export::ExportFormat::parse(&format)?;
                let path =
                    varynth::export::export_session(&session, format, out.as_deref(), force)?;
                println!("{}", path.display());
            }
        },
        Commands::Channels => {
            println!("{}", serde_json::to_string_pretty(&channels::v2_status())?);
        }
        Commands::Notify { command } => match command {
            NotifyCommand::Status => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&varynth::notify::capability(&cfg))?
                );
            }
            NotifyCommand::Test { label } => {
                let reports = varynth::notify::send_test(&label);
                let sent = reports.iter().any(|report| report.ok);
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "sent": sent,
                        "mode": varynth::notify::capability(&cfg)["mode"],
                        "reports": reports,
                    }))?
                );
                if !sent {
                    anyhow::bail!("no notification mechanism succeeded");
                }
            }
        },
        Commands::Task { command } => match command {
            TaskCommand::Add {
                name,
                prompt,
                at,
                every_seconds,
                cron,
                allow_background_tools,
            } => {
                let mut store = TaskStore::load()?;
                let trigger =
                    parse_trigger_with_cron(at.as_deref(), every_seconds, cron.as_deref())?;
                let task = store.add(name, prompt, trigger, allow_background_tools)?;
                println!("{}", serde_json::to_string_pretty(&task)?);
            }
            TaskCommand::List => {
                let store = TaskStore::load()?;
                println!("{}", serde_json::to_string_pretty(store.list())?);
            }
            TaskCommand::Disable { id } => {
                TaskStore::load()?.set_enabled(&id, false)?;
            }
            TaskCommand::Enable { id } => {
                TaskStore::load()?.set_enabled(&id, true)?;
            }
            TaskCommand::Remove { id } => {
                TaskStore::load()?.remove(&id)?;
            }
            TaskCommand::Run { id } => {
                run_task(&cfg, &cwd, &id).await?;
            }
        },
        Commands::Serve { host, port } => {
            if let Some(h) = host {
                cfg.dashboard_host = h;
            }
            if let Some(p) = port {
                cfg.dashboard_port = p;
            }
            dashboard::serve(cfg, cwd).await?;
        }
        Commands::Exec { prompt, output } => {
            let rt = Runtime::new(cfg.clone(), cwd.clone())?;
            let session = Session::new(&cwd.display().to_string(), &cfg.model)?;
            run_exec(rt, session, prompt, output).await?;
        }
        Commands::Resume { id } => {
            let rt = Runtime::new(cfg, cwd)?;
            let session = match id {
                Some(id) => Session::load(&id)?,
                None => Session::load_latest()?.context("no sessions")?,
            };
            start_ui(rt, session, cli.plain).await?;
        }
        Commands::Theme { command } => handle_theme(command, &mut cfg)?,
        Commands::Config { command } => handle_config(command, &mut cfg)?,
        Commands::Tools { json } => handle_tools(json)?,
        Commands::Computer { command } => handle_computer(command, &mut cfg)?,
        Commands::Endpoint { command } => handle_endpoint(command, &mut cfg)?,
        Commands::Decisions { command } => {
            let value = match command {
                DecisionsCommand::Status => {
                    serde_json::to_value(varynth::decisions::status(&cfg).await)?
                }
                DecisionsCommand::Start => {
                    serde_json::to_value(varynth::decisions::start(&cfg).await?)?
                }
                DecisionsCommand::Stop => serde_json::to_value(varynth::decisions::stop()?)?,
            };
            println!("{}", serde_json::to_string_pretty(&value)?);
        }
        Commands::Login { no_browser } => {
            varynth_auth::login(&cfg.varynth_api_url, &cfg.varynth_web_url, no_browser).await?;
        }
        Commands::Logout => {
            varynth_auth::logout(&cfg.varynth_api_url).await?;
        }
        Commands::Decide {
            json,
            file,
            state,
            questions,
            engine,
        } => {
            let mut request = if let Some(text) = json {
                varynth::decisions::parse_request(&text)?
            } else if let Some(path) = file {
                varynth::decisions::load_request_file(&path)?
            } else {
                let state = state.expect("clap requires --state without --json/--file");
                let questions = questions.expect("clap requires --questions without --json/--file");
                let questions: serde_json::Map<String, serde_json::Value> =
                    serde_json::from_str(&questions)
                        .context("--questions must be a JSON object of question definitions")?;
                varynth::decisions::DecisionRequest::new(
                    varynth::decisions::parse_state(&state),
                    questions,
                )
            };
            request.engine = engine.or(request.engine);
            // Local service down + loopback + autostart on: spawn it, wait for
            // health, then send the request. Autostart is skipped otherwise.
            let health = varynth::decisions::ensure_running(&cfg).await?;
            if !health.both() {
                eprintln!(
                    "note: decisions service is up but not every engine is ready ({})",
                    health.detail()
                );
            }
            let response = varynth::decisions::decide(&cfg, &request).await?;
            println!("{}", serde_json::to_string_pretty(&response)?);
        }
        Commands::Review { staged } => {
            run_review(cfg, cwd, staged).await?;
        }
        Commands::Swarm { task } => {
            run_swarm(cfg, cwd, task).await?;
        }
        Commands::Sync => {
            run_cloud_sync(&cfg).await?;
        }
        Commands::Repl => {
            let rt = Runtime::new(cfg.clone(), cwd.clone())?;
            let session = Session::new(&cwd.display().to_string(), &cfg.model)?;
            start_ui(rt, session, cli.plain).await?;
        }
    }
    Ok(())
}

async fn run_review(mut cfg: Config, cwd: PathBuf, staged: bool) -> Result<()> {
    let advisor_model = cfg.roles.advisor.clone().unwrap_or_else(|| cfg.model.clone());
    cfg.model = advisor_model.clone();
    eprintln!(".. varynth review Â· role=advisor Â· model={advisor_model}");

    let diff_args: &[&str] = if staged {
        &["diff", "--staged"]
    } else {
        &["diff", "HEAD"]
    };
    let diff_out = std::process::Command::new("git")
        .args(diff_args)
        .current_dir(&cwd)
        .output();

    let diff_text = match diff_out {
        Ok(out) => String::from_utf8_lossy(&out.stdout).to_string(),
        Err(_) => String::new(),
    };

    let status_out = std::process::Command::new("git")
        .args(["status", "--short"])
        .current_dir(&cwd)
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();

    let truncated_diff: String = diff_text.chars().take(18_000).collect();
    let review_prompt = format!(
        "Perform a rigorous automated Git Code Review and Pull Request audit on this workspace.\n\
         1. Critical Bugs & Logic Errors\n\
         2. Security & Injection / Auth Risks\n\
         3. Concurrency & Performance Bottlenecks\n\
         4. Ready-to-paste Pull Request Summary\n\n\
         Git status:\n```\n{status_out}\n```\n\nGit diff:\n```diff\n{truncated_diff}\n```"
    );

    let rt = Runtime::new(cfg.clone(), cwd.clone())?;
    let session = Session::new(&cwd.display().to_string(), &cfg.model)?;
    run_exec(rt, session, review_prompt, OutputMode::Text).await
}

async fn run_swarm(cfg: Config, cwd: PathBuf, task: String) -> Result<()> {
    let plan_model = cfg.roles.plan.clone().unwrap_or_else(|| cfg.model.clone());
    let builder_model = cfg.roles.default.clone().unwrap_or_else(|| cfg.model.clone());
    let advisor_model = cfg.roles.advisor.clone().unwrap_or_else(|| cfg.model.clone());

    eprintln!("== Varynth Multi-Agent Swarm ==");
    eprintln!("   [1/3] Architect (plan)    : {plan_model}");
    eprintln!("   [2/3] Builder   (default) : {builder_model}");
    eprintln!("   [3/3] Auditor   (advisor) : {advisor_model}");

    // Stage 1: Architect
    let mut plan_cfg = cfg.clone();
    plan_cfg.model = plan_model;
    let mut plan_rt = Runtime::new(plan_cfg.clone(), cwd.clone())?;
    let mut plan_session = Session::new(&cwd.display().to_string(), &plan_cfg.model)?;
    let plan_prompt = format!(
        "[SWARM STAGE 1 Â· ARCHITECT]\nDecompose the following engineering task into a concise, step-by-step execution blueprint for the Builder agent:\n\nTask: {task}"
    );
    eprintln!("\n-- Stage 1/3: Architect decomposing task...");
    let blueprint = plan_rt
        .turn(&mut plan_session, &plan_prompt, |ev| {
            if ev.kind != "assistant" && ev.kind != "delta" {
                eprintln!("   [architect:{}] {}", ev.kind, ev.text);
            }
        })
        .await?;
    println!("\n### [Swarm 1/3 Â· Architect Blueprint]\n{blueprint}\n");

    // Stage 2: Builder
    let mut build_cfg = cfg.clone();
    build_cfg.model = builder_model;
    let mut build_rt = Runtime::new(build_cfg.clone(), cwd.clone())?;
    let mut build_session = Session::new(&cwd.display().to_string(), &build_cfg.model)?;
    let build_prompt = format!(
        "[SWARM STAGE 2 Â· BUILDER]\nExecute the following task using the Architect's blueprint:\n\nTask: {task}\n\nBlueprint:\n{blueprint}"
    );
    eprintln!("-- Stage 2/3: Builder executing implementation...");
    let implementation = build_rt
        .turn(&mut build_session, &build_prompt, |ev| {
            if ev.kind != "assistant" && ev.kind != "delta" {
                eprintln!("   [builder:{}] {}", ev.kind, ev.text);
            }
        })
        .await?;
    println!("\n### [Swarm 2/3 Â· Builder Execution]\n{implementation}\n");

    // Stage 3: Auditor
    let mut audit_cfg = cfg.clone();
    audit_cfg.model = advisor_model;
    let mut audit_rt = Runtime::new(audit_cfg.clone(), cwd.clone())?;
    let mut audit_session = Session::new(&cwd.display().to_string(), &audit_cfg.model)?;
    let audit_prompt = format!(
        "[SWARM STAGE 3 Â· AUDITOR]\nVerify the Builder's execution against the original task and highlight any edge cases or follow-up checks:\n\nTask: {task}\n\nBuilder report:\n{implementation}"
    );
    eprintln!("-- Stage 3/3: Auditor verifying output...");
    let audit = audit_rt
        .turn(&mut audit_session, &audit_prompt, |ev| {
            if ev.kind != "assistant" && ev.kind != "delta" {
                eprintln!("   [auditor:{}] {}", ev.kind, ev.text);
            }
        })
        .await?;
    println!("\n### [Swarm 3/3 Â· Auditor Verification]\n{audit}");
    Ok(())
}

async fn run_cloud_sync(cfg: &Config) -> Result<()> {
    let tokens = varynth_auth::load_auth_env()?
        .context("not signed in to Varynth Cloud; run `varynth login` first")?;
    let sessions = Session::list()?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?;
    let endpoint = format!(
        "{}/v1/me/sessions",
        cfg.varynth_api_url.trim_end_matches('/')
    );

    let mut synced = 0usize;
    for meta in sessions.into_iter().take(15) {
        let loaded = Session::load(&meta.id).ok();
        let msg_count = loaded.as_ref().map(|s| s.messages.len() as i32).unwrap_or(0);
        let preview = loaded
            .as_ref()
            .and_then(|s| s.messages.last())
            .map(|m| m.content.chars().take(240).collect::<String>())
            .unwrap_or_default();

        let resp = client
            .post(&endpoint)
            .bearer_auth(&tokens.access_token)
            .json(&serde_json::json!({
                "session_key": meta.id,
                "title": meta.title,
                "model": meta.model,
                "workspace": meta.cwd,
                "message_count": msg_count,
                "preview": preview,
            }))
            .send()
            .await?;

        if resp.status().is_success() {
            synced += 1;
        }
    }
    println!(
        "synced {synced} session(s) to {}/dashboard",
        cfg.varynth_web_url.trim_end_matches('/')
    );
    Ok(())
}

fn handle_endpoint(command: EndpointCommand, cfg: &mut Config) -> Result<()> {
    match command {
        EndpointCommand::Add {
            id,
            kind,
            base_url,
            secret,
        } => {
            accounts::link_custom(cfg, &id, &kind, &base_url)?;
            if let Some(secret) = secret {
                let secret = if secret == "-" {
                    read_stdin_secret()?
                } else {
                    eprintln!("secret was passed as an argument; prefer stdin `-`");
                    secret
                };
                validate_secret(&secret)?;
                profile_keyring_set(id.trim(), &secret)?;
            }
            cfg.select_endpoint(&id)?;
            cfg.save()?;
            eprintln!(
                "endpoint {} saved ({kind}, {})",
                id.trim(),
                base_url.trim()
            );
        }
        EndpointCommand::List => {
            for profile in cfg.provider_profiles.iter().filter(|profile| {
                profile.keyring_field.is_none()
                    && profile.api_key.is_none()
                    && matches!(profile.kind.as_str(), "openai" | "anthropic")
                    && !accounts::ACCOUNTS
                        .iter()
                        .any(|account| account.id == profile.id)
            }) {
                println!(
                    "{}\t{}\t{}",
                    profile.id,
                    profile.kind,
                    profile.base_url.as_deref().unwrap_or("")
                );
            }
        }
        EndpointCommand::Remove { id } => {
            anyhow::ensure!(
                !accounts::ACCOUNTS.iter().any(|account| account.id == id),
                "use `varynth account disconnect {id}` for official accounts"
            );
            cfg.provider_profiles.retain(|profile| profile.id != id);
            if cfg.provider == id {
                cfg.provider = "proxy".into();
            }
            profile_keyring_delete(&id)?;
            cfg.validate()?;
            cfg.save()?;
            eprintln!("endpoint {id} removed");
        }
    }
    Ok(())
}

fn normalize_sandbox(value: &str) -> Result<String> {
    let value = value.trim().to_ascii_lowercase();
    let normalized = match value.as_str() {
        "docker" => "docker-isolated",
        "read-only" | "workspace-write" | "danger-full-access" | "docker-isolated" => &value,
        _ => anyhow::bail!("invalid sandbox `{value}` (expected read-only, workspace-write, danger-full-access, docker-isolated, or docker)"),
    };
    Ok(normalized.to_string())
}

fn handle_account(command: AccountCommand, cfg: &mut Config) -> Result<()> {
    match command {
        AccountCommand::Status => {
            let rows: Vec<_> = accounts::ACCOUNTS
                .iter()
                .map(|account| {
                    let linked = keyring_get(account.keyring_field)
                        .map(|value| value.is_some_and(|secret| !secret.trim().is_empty()))
                        .unwrap_or(false);
                    serde_json::json!({
                        "id": account.id,
                        "label": account.label,
                        "linked": linked,
                        "use": accounts::usage_label(account),
                        "note": account.note,
                    })
                })
                .collect();
            println!("{}", serde_json::to_string(&rows)?);
        }
        AccountCommand::Connect {
            provider,
            secret,
            account_id,
        } => {
            let account = accounts::reject_unknown_provider_name(&provider)?;
            let secret = match secret.as_deref() {
                Some("-") => Some(read_stdin_secret()?),
                Some(value) => {
                    eprintln!("secret was passed as an argument; prefer stdin `-`");
                    Some(value.to_string())
                }
                None => None,
            };
            if let Some(secret) = secret.as_deref() {
                validate_secret(secret)?;
            } else {
                let present = keyring_get(account.keyring_field)?
                    .is_some_and(|value| !value.trim().is_empty());
                anyhow::ensure!(present, "secret required; pass `-` on stdin");
            }
            let mut next = cfg.clone();
            accounts::link_config(&mut next, account, account_id.as_deref())?;
            next.validate()?;
            if let Some(secret) = secret.as_deref() {
                keyring_set(account.keyring_field, secret)?;
            }
            accounts::link_config(cfg, account, account_id.as_deref())?;
            cfg.save()?;
            eprintln!("linked {} ({})", account.id, accounts::usage_label(account));
        }
        AccountCommand::Disconnect { provider } => {
            let account = accounts::reject_unknown_provider_name(&provider)?;
            accounts::unlink_config(cfg, account);
            cfg.validate()?;
            cfg.save()?;
            keyring_delete(account.keyring_field)?;
            eprintln!("unlinked {}", account.id);
        }
    }
    Ok(())
}

fn handle_keyring(command: KeyringCommand) -> Result<()> {
    match command {
        KeyringCommand::Set { field, value } => {
            let field = normalize_keyring_field(&field)?;
            let value = match value.as_deref() {
                Some("-") => read_stdin_secret()?,
                Some(value) => value.to_string(),
                None => read_masked_secret(field)?,
            };
            validate_secret(&value)?;
            keyring_set(field, &value)?;
            eprintln!("stored keyring field `{field}` without printing its value");
        }
        KeyringCommand::Get { field } => {
            let field = normalize_keyring_field(&field)?;
            match keyring_get(field)? {
                Some(value) => {
                    eprintln!("WARNING: printing secret keyring field `{field}` to stdout");
                    println!("{value}");
                }
                None => anyhow::bail!("keyring field `{field}` is not set"),
            }
        }
        KeyringCommand::Delete { field } => {
            let field = normalize_keyring_field(&field)?;
            keyring_delete(field)?;
            eprintln!("deleted keyring field `{field}`");
        }
        KeyringCommand::Status => {
            let fields: Vec<_> = KEYRING_FIELDS
                .into_iter()
                .map(|field| {
                    keyring_get(field)
                        .map(|value| serde_json::json!({"field":field,"present":value.is_some()}))
                })
                .collect::<Result<_>>()?;
            println!("{}", serde_json::to_string(&fields)?);
        }
    }
    Ok(())
}

fn validate_secret(value: &str) -> Result<()> {
    anyhow::ensure!(!value.trim().is_empty(), "keyring value cannot be empty");
    anyhow::ensure!(
        !value.chars().any(char::is_control),
        "keyring value must be a single line without control characters"
    );
    Ok(())
}

fn read_stdin_secret() -> Result<String> {
    let mut value = String::new();
    io::stdin().read_to_string(&mut value)?;
    Ok(value.trim_end_matches(['\r', '\n']).to_string())
}

fn read_masked_secret(field: &str) -> Result<String> {
    if !io::stdin().is_terminal() {
        return read_stdin_secret();
    }
    struct RawMode;
    impl Drop for RawMode {
        fn drop(&mut self) {
            let _ = crossterm::terminal::disable_raw_mode();
            eprintln!();
        }
    }
    eprint!("Enter value for {field}: ");
    io::stderr().flush()?;
    crossterm::terminal::enable_raw_mode()?;
    let _raw_mode = RawMode;
    let mut value = String::new();
    loop {
        let crossterm::event::Event::Key(key) = crossterm::event::read()? else {
            continue;
        };
        if key.kind == crossterm::event::KeyEventKind::Release {
            continue;
        }
        if key.code == crossterm::event::KeyCode::Esc
            || (key
                .modifiers
                .contains(crossterm::event::KeyModifiers::CONTROL)
                && matches!(key.code, crossterm::event::KeyCode::Char('c' | 'd')))
        {
            anyhow::bail!("keyring input cancelled");
        }
        match key.code {
            crossterm::event::KeyCode::Enter => return Ok(value),
            crossterm::event::KeyCode::Backspace => {
                value.pop();
            }
            crossterm::event::KeyCode::Char(ch) if !ch.is_control() => value.push(ch),
            _ => {}
        }
    }
}

async fn handle_skill(command: SkillCommand) -> Result<()> {
    match command {
        SkillCommand::List => {
            let entries = skills_search::list_installed(&Config::home_dir().join("skills"));
            println!("{}", serde_json::to_string_pretty(&entries)?);
        }
        SkillCommand::Search { query } => {
            let entries = tokio::task::spawn_blocking(move || skills_search::search(&query))
                .await
                .context("skill search task failed")??;
            println!("{}", serde_json::to_string_pretty(&entries)?);
        }
        SkillCommand::Install { source, force } => {
            let report = tokio::task::spawn_blocking(move || {
                let source = skills_search::resolve_source(&source)?;
                let dir = Config::home_dir().join("skills");
                skills_search::install_to(&source, &dir, force)
            })
            .await
            .context("skill install task failed")??;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
    }
    Ok(())
}

/// `varynth commands list`: the user-defined command surface, including why
/// a command file was skipped and whether project files are inert.
fn print_commands(cwd: &Path, command: CommandsCommand) {
    let set = varynth::commands::CommandSet::discover(cwd);
    let CommandsCommand::List { json } = command;
    for warning in &set.warnings {
        eprintln!("!! {warning}");
    }
    if set.project_pending {
        eprintln!(
            "!! project commands ignored: workspace not trusted (run `varynth trust`)  Â·  {}",
            varynth::commands::CommandSet::project_dir(cwd).display()
        );
    }
    if json {
        let rows: Vec<serde_json::Value> = set
            .commands
            .iter()
            .map(|command| {
                serde_json::json!({
                    "name": command.name,
                    "description": command.description,
                    "argumentHint": command.argument_hint,
                    "source": if command.project { "project" } else { "user" },
                    "path": command.path,
                })
            })
            .collect();
        match serde_json::to_string_pretty(&rows) {
            Ok(text) => println!("{text}"),
            Err(error) => eprintln!("!! {error}"),
        }
        return;
    }
    if set.commands.is_empty() {
        println!(
            "(no user-defined commands)\nadd {} or {}",
            varynth::commands::CommandSet::user_dir()
                .join("<name>.md")
                .display(),
            varynth::commands::CommandSet::project_dir(cwd)
                .join("<name>.md")
                .display()
        );
        return;
    }
    for command in &set.commands {
        println!(
            "/{}  {}  {}{}",
            command.name,
            if command.project { "project" } else { "user" },
            command.description,
            command
                .argument_hint
                .as_deref()
                .map(|hint| format!("  Â·  {hint}"))
                .unwrap_or_default()
        );
    }
}

fn handle_mcp(command: McpCommand, configured: Option<&Path>) -> Result<()> {
    if let McpCommand::Serve { name } = &command {
        if name != "web-search" {
            anyhow::bail!("unknown mcp server `{name}`");
        }
        return varynth::web_search_mcp::serve();
    }
    let mut manager = mcp::McpManager::from_config(configured)?;
    match command {
        McpCommand::Status => println!("{}", serde_json::to_string_pretty(&manager.status())?),
        McpCommand::Probe { name } => println!(
            "{}",
            serde_json::to_string_pretty(&manager.probe_server(&name)?)?
        ),
        McpCommand::Ping { name } => {
            println!("{}", serde_json::to_string_pretty(&manager.ping(&name)?)?)
        }
        McpCommand::Serve { .. } => unreachable!("serve returns before the manager"),
    }
    Ok(())
}

const GOAL_CONTINUATION: &str = "Goal still active. Continue uninterrupted. Do not ask what to do. End with a line that is exactly GOAL_COMPLETE only when the condition is fully met.";

fn goal_progress(runtime: &Runtime) -> Option<u32> {
    runtime.goal.as_ref().map(|goal| goal.rounds)
}

fn event_envelope(
    event: &str,
    session: Option<&str>,
    seq: u64,
    data: serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({"schema_version":1,"event":event,"session_id":session,"seq":seq,"data":data})
}

fn emit_exec_event(
    event: &str,
    session: Option<&str>,
    seq: &mut u64,
    data: serde_json::Value,
) -> Result<()> {
    *seq += 1;
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "{}", event_envelope(event, session, *seq, data))?;
    stdout.flush()?;
    Ok(())
}

async fn run_exec(
    mut runtime: Runtime,
    mut session: Session,
    prompt: String,
    output: OutputMode,
) -> Result<()> {
    let mut next = prompt;
    let mut seq = 0;
    let sid = session.id().to_string();
    let mut turn_index = 0u64;
    let final_reply = loop {
        turn_index += 1;
        let previous_rounds = goal_progress(&runtime);
        let mut streamed = false;
        let mut output_error = None;
        let result = runtime
            .turn(&mut session, &next, |event| {
                if output_error.is_some() {
                    return;
                }
                match output {
                    OutputMode::Text => {
                        if event.kind != "assistant" && event.kind != "delta" {
                            eprintln!("[{}] {}", event.kind, event.text);
                        }
                    }
                    OutputMode::Json => {}
                    OutputMode::StreamJson => {
                        if event.kind == "assistant" {
                            return;
                        }
                        streamed |= event.kind == "delta";
                        if let Err(error) = emit_exec_event(
                            &event.kind,
                            Some(&sid),
                            &mut seq,
                            serde_json::json!({"text":event.text,"turn":turn_index}),
                        ) {
                            output_error = Some(error);
                        }
                    }
                }
            })
            .await;
        if let Some(error) = output_error {
            return Err(error);
        }
        let reply = match result {
            Ok(reply) => reply,
            Err(error) => {
                if output != OutputMode::Text {
                    emit_exec_event(
                        "error",
                        Some(&sid),
                        &mut seq,
                        serde_json::json!({"ok":false,"message":error.to_string()}),
                    )?;
                }
                return Err(error);
            }
        };
        if output == OutputMode::StreamJson {
            emit_exec_event(
                "result",
                Some(&sid),
                &mut seq,
                serde_json::json!({
                    "ok":true,"turn":turn_index,"streamed":streamed,
                    "text":if streamed { "" } else { &reply }
                }),
            )?;
        }
        if !runtime.wants_goal_continue() {
            break reply;
        }
        if goal_progress(&runtime) == previous_rounds {
            // A paused session or a local slash command must not spin forever.
            break reply;
        }
        if output == OutputMode::Text {
            eprintln!("[system] goal still active - continuing");
        }
        next = GOAL_CONTINUATION.into();
    };
    match output {
        OutputMode::Text => println!("{final_reply}"),
        OutputMode::Json => emit_exec_event(
            "result",
            Some(&sid),
            &mut seq,
            serde_json::json!({"ok":true,"text":final_reply}),
        )?,
        OutputMode::StreamJson => {
            emit_exec_event("done", Some(&sid), &mut seq, serde_json::json!({"ok":true}))?
        }
    }
    Ok(())
}

async fn run_task(cfg: &Config, cwd: &Path, id: &str) -> Result<()> {
    let mut store = TaskStore::load()?;
    let task = store.find(id).context("task not found")?.clone();
    anyhow::ensure!(task.enabled, "task is disabled");
    let run_id = store.claim(id, Utc::now())?;
    let mut task_cfg = cfg.clone();
    if !task.allow_background_tools {
        task_cfg.permission_mode = "prompt".into();
    }
    let mut runtime = Runtime::new(task_cfg.clone(), cwd.to_path_buf())?;
    let mut session = Session::new(&cwd.display().to_string(), &task_cfg.model)?;
    let mut next = task.prompt;
    let reply = loop {
        let result = runtime
            .turn(&mut session, &next, |event| {
                if event.kind != "assistant" && event.kind != "delta" {
                    eprintln!("[{}] {}", event.kind, event.text);
                }
            })
            .await;
        match result {
            Ok(_reply) if runtime.wants_goal_continue() => {
                eprintln!("[system] goal still active - continuing");
                next = "Goal still active. Continue uninterrupted. Do not ask what to do. End with a line that is exactly GOAL_COMPLETE only when the condition is fully met.".into();
            }
            Ok(reply) => break reply,
            Err(error) => {
                store.release(id, &run_id)?;
                return Err(error);
            }
        }
    };
    if let Err(error) = store.mark_run(id, &run_id, Utc::now()) {
        store.release(id, &run_id)?;
        return Err(error);
    }
    println!("{reply}");
    Ok(())
}

async fn start_ui(rt: Runtime, session: Session, plain: bool) -> Result<()> {
    if plain || !io::stdout().is_terminal() {
        return repl(rt, session).await;
    }
    tui::run(rt, session).await
}

async fn repl(mut rt: Runtime, mut session: Session) -> Result<()> {
    println!(
        "varynth  {}  model={}  sandbox={}  session={}",
        rt.jail.cwd.display(),
        rt.cfg.model,
        rt.cfg.sandbox,
        session.id()
    );
    println!("slash: /status /autocompact /compact /models /skills /review /goal /send /inbox /undo [n] /redo /checkpoints /remind /serve /quit   skill: /name");
    let mut rl = DefaultEditor::new()?;
    loop {
        let line = match rl.readline("varynth â€º ") {
            Ok(l) => l,
            Err(_) => break,
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let _ = rl.add_history_entry(line);
        match line {
            "/quit" | "/exit" => break,
            "/status" => println!(
                "{}",
                serde_json::to_string_pretty(&rt.status_json(Some(&session)))?
            ),
            "/models" => match rt.list_models().await {
                Ok(ms) => {
                    for m in ms {
                        println!("{}", m.id);
                    }
                }
                Err(e) => eprintln!("{e}"),
            },
            "/skills" => {
                for s in &rt.skills {
                    println!("/{}  {}", s.name, s.description);
                }
            }
            "/serve" => println!(
                "run `varynth serve` in another terminal - dashboard http://{}:{}",
                rt.cfg.dashboard_host, rt.cfg.dashboard_port
            ),
            "/review" => {
                let cwd = rt.jail.cwd.clone();
                match rt.review(&mut session, &cwd).await {
                    Ok(text) => println!("{text}"),
                    Err(e) => eprintln!("error: {e}"),
                }
            }
            other if other.starts_with("/remind ") => {
                let rest = other.trim_start_matches("/remind ").trim();
                let mut parts = rest.splitn(2, ' ');
                let seconds = parts
                    .next()
                    .and_then(|value| value.parse::<i64>().ok())
                    .context("usage: /remind <seconds> <message>")?;
                let prompt = parts
                    .next()
                    .filter(|value| !value.trim().is_empty())
                    .context("usage: /remind <seconds> <message>")?;
                anyhow::ensure!(seconds > 0, "reminder seconds must be greater than zero");
                let mut store = TaskStore::load()?;
                let task = store.add(
                    "reminder".into(),
                    prompt.into(),
                    Trigger::At {
                        at: Utc::now() + Duration::seconds(seconds),
                    },
                    false,
                )?;
                println!("reminder created: {}", task.id);
            }
            other => {
                let mut next = other.to_string();
                loop {
                    match rt
                        .turn(&mut session, &next, |ev| {
                            if ev.kind == "tool" {
                                eprintln!("  â–¸ {}", ev.text);
                            }
                        })
                        .await
                    {
                        Ok(reply) => {
                            println!("{reply}");
                            if !rt.wants_goal_continue() {
                                break;
                            }
                            eprintln!("[system] goal still active - continuing");
                            next = "Goal still active. Continue uninterrupted. Do not ask what to do. End with a line that is exactly GOAL_COMPLETE only when the condition is fully met.".into();
                        }
                        Err(e) => {
                            eprintln!("error: {e}");
                            break;
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// Non-secret settings `varynth config` may read and write. Anything outside
/// this list (including every keyring field and `dashboard_token`) is refused
/// so a typo can never persist a credential in config.toml.
const CONFIG_KEYS: [&str; 16] = [
    "model",
    "provider",
    "permission_mode",
    "sandbox",
    "theme",
    "effort",
    "auto_compact",
    "auto_compact_percent",
    "notifications",
    "decisions_autostart",
    "decisions_url",
    "computer_use",
    "dashboard_host",
    "dashboard_port",
    "hooks_timeout_secs",
    "hooks_timeout_policy",
];

/// Settings `config get` can also print even though `set` does not own them.
const CONFIG_READ_ONLY_KEYS: [&str; 2] = ["varynth_api_url", "varynth_web_url"];

fn config_value(cfg: &Config, key: &str) -> Option<serde_json::Value> {
    Some(match key {
        "model" => cfg.model.clone().into(),
        "provider" => cfg.provider.clone().into(),
        "permission_mode" => cfg.permission_mode.clone().into(),
        "sandbox" => cfg.sandbox.clone().into(),
        "theme" => cfg.theme.clone().into(),
        "effort" => cfg.effort.clone().into(),
        "auto_compact" => cfg.auto_compact.into(),
        "auto_compact_percent" => cfg.auto_compact_percent.into(),
        "notifications" => cfg.notifications.clone().into(),
        "decisions_autostart" => cfg.decisions_autostart.into(),
        "decisions_url" => cfg.decisions_url.clone().into(),
        "computer_use" => cfg.computer_use.into(),
        "dashboard_host" => cfg.dashboard_host.clone().into(),
        "dashboard_port" => cfg.dashboard_port.into(),
        "hooks_timeout_secs" => cfg.hooks_timeout_secs.into(),
        "hooks_timeout_policy" => cfg.hooks_timeout_policy.clone().into(),
        "varynth_api_url" => cfg.varynth_api_url.clone().into(),
        "varynth_web_url" => cfg.varynth_web_url.clone().into(),
        _ => return None,
    })
}

fn scalar_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn parse_bool(raw: &str) -> Result<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => anyhow::bail!("expected true or false"),
    }
}

fn validate_http_url(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    let url = reqwest::Url::parse(trimmed).with_context(|| format!("invalid URL `{trimmed}`"))?;
    anyhow::ensure!(
        matches!(url.scheme(), "http" | "https"),
        "URL must use http or https"
    );
    anyhow::ensure!(
        url.username().is_empty() && url.password().is_none(),
        "URL must not embed credentials; put secrets in `varynth keyring`"
    );
    anyhow::ensure!(
        url.query().is_none() && url.fragment().is_none(),
        "URL must not carry a query or fragment; keep it a plain base URL"
    );
    Ok(trimmed.to_string())
}

/// Validate and apply one setting. Pure with respect to I/O so the refusal and
/// validation rules are unit-testable; the caller persists with `cfg.save()`.
fn set_config_value(cfg: &mut Config, key: &str, raw: &str) -> Result<()> {
    if !CONFIG_KEYS.contains(&key) {
        anyhow::bail!(
            "`{key}` is not a writable setting; secrets belong in `varynth keyring set <field>`"
        );
    }
    let raw = raw.trim();
    anyhow::ensure!(!raw.is_empty(), "`{key}` needs a value");
    match key {
        "model" => cfg.select_model(raw)?,
        "provider" => {
            cfg.provider = raw.to_string();
        }
        "permission_mode" => {
            anyhow::ensure!(
                matches!(raw, "acceptEdits" | "prompt" | "bypass" | "plan"),
                "permission_mode must be acceptEdits, prompt, bypass, or plan"
            );
            cfg.permission_mode = raw.to_string();
        }
        "sandbox" => cfg.sandbox = normalize_sandbox(raw)?,
        "theme" => cfg.theme = varynth::theme::canonical(raw),
        "effort" => cfg.effort = raw.to_ascii_lowercase(),
        "auto_compact" => cfg.auto_compact = parse_bool(raw)?,
        "auto_compact_percent" => {
            let percent: u8 = raw
                .parse()
                .context("auto_compact_percent must be a number")?;
            anyhow::ensure!(
                (1..=99).contains(&percent),
                "auto_compact_percent must be between 1 and 99"
            );
            cfg.auto_compact_percent = percent;
        }
        "notifications" => {
            anyhow::ensure!(
                matches!(raw, "off" | "auto"),
                "notifications must be off or auto"
            );
            cfg.notifications = raw.to_string();
        }
        "decisions_autostart" => cfg.decisions_autostart = parse_bool(raw)?,
        "decisions_url" => cfg.decisions_url = validate_http_url(raw)?,
        "computer_use" => cfg.computer_use = parse_bool(raw)?,
        "dashboard_host" => {
            anyhow::ensure!(
                raw.parse::<std::net::IpAddr>().is_ok() || raw == "localhost",
                "dashboard_host must be an IP address or localhost"
            );
            cfg.dashboard_host = raw.to_string();
        }
        "dashboard_port" => {
            let port: u16 = raw.parse().context("dashboard_port must be a number")?;
            anyhow::ensure!(port >= 1, "dashboard_port must be between 1 and 65535");
            cfg.dashboard_port = port;
        }
        "hooks_timeout_secs" => {
            cfg.hooks_timeout_secs = raw.parse().context("hooks_timeout_secs must be a number")?
        }
        "hooks_timeout_policy" => {
            anyhow::ensure!(
                matches!(raw, "deny" | "ask"),
                "hooks_timeout_policy must be deny or ask"
            );
            cfg.hooks_timeout_policy = raw.to_string();
        }
        other => unreachable!("allowlist and match arms diverge for {other}"),
    }
    cfg.validate()?;
    Ok(())
}

fn config_usage_error(message: &str) -> ! {
    eprintln!("error: {message}");
    std::process::exit(2);
}

/// Resolve an editor program without the Windows CWD-first search order: an
/// absolute path is used as-is, a bare name is looked up on PATH only. A
/// workspace-supplied `code.exe` can therefore never be executed because the
/// user happened to run `varynth config edit` inside a cloned repository.
fn resolve_editor_program(program: &str) -> Result<std::path::PathBuf> {
    let path = Path::new(program);
    if path.is_absolute() {
        anyhow::ensure!(path.is_file(), "editor not found: {}", path.display());
        return Ok(path.to_path_buf());
    }
    anyhow::ensure!(
        !program.contains('/') && !program.contains('\\'),
        "EDITOR must be a bare command on PATH or an absolute path"
    );
    let extensions: &[&str] = if cfg!(windows) {
        &["exe", "com", "bat", "cmd"]
    } else {
        &[""]
    };
    let search = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&search) {
        for extension in extensions {
            let candidate = if extension.is_empty() {
                dir.join(program)
            } else {
                dir.join(format!("{program}.{extension}"))
            };
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    anyhow::bail!("editor `{program}` was not found on PATH; set EDITOR to an absolute path")
}

/// The on-disk config, without `apply_env` or CLI overrides. `config set` must
/// never persist an environment-derived secret, so it edits this copy rather
/// than the process config.
fn load_disk_config() -> Result<Config> {
    let path = Config::config_path();
    if !path.exists() {
        return Ok(Config::default());
    }
    let raw = std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    toml::from_str(&raw).context("parse config.toml")
}

fn handle_config(command: ConfigCommand, cfg: &mut Config) -> Result<()> {
    match command {
        ConfigCommand::Get { key, json } => {
            if let Some(key) = key {
                let key = key.trim();
                let Some(value) = config_value(cfg, key) else {
                    config_usage_error(&format!(
                        "unknown setting `{key}`; readable settings: {}",
                        CONFIG_KEYS
                            .iter()
                            .chain(CONFIG_READ_ONLY_KEYS.iter())
                            .copied()
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                };
                if json {
                    println!("{}", serde_json::json!({ key: value }));
                } else {
                    println!("{key}={}", scalar_text(&value));
                }
                return Ok(());
            }
            if json {
                let mut map = serde_json::Map::new();
                for key in CONFIG_KEYS.iter().chain(CONFIG_READ_ONLY_KEYS.iter()) {
                    if let Some(value) = config_value(cfg, key) {
                        map.insert((*key).to_string(), value);
                    }
                }
                println!("{}", serde_json::Value::Object(map));
            } else {
                for key in CONFIG_KEYS.iter().chain(CONFIG_READ_ONLY_KEYS.iter()) {
                    if let Some(value) = config_value(cfg, key) {
                        println!("{key}={}", scalar_text(&value));
                    }
                }
            }
        }
        ConfigCommand::Set { key, value } => {
            let key = key.trim().to_string();
            let mut disk = match load_disk_config() {
                Ok(cfg) => cfg,
                Err(err) => config_usage_error(&format!("{err}")),
            };
            if let Err(err) = set_config_value(&mut disk, &key, &value) {
                config_usage_error(&format!("{err}"));
            }
            disk.save()?;
            println!(
                "{key}={}",
                config_value(&disk, &key)
                    .map(|value| scalar_text(&value))
                    .unwrap_or_default()
            );
        }
        ConfigCommand::Edit => {
            let editor = std::env::var("VISUAL")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .or_else(|| {
                    std::env::var("EDITOR")
                        .ok()
                        .filter(|value| !value.trim().is_empty())
                });
            let Some(editor) = editor else {
                println!("{}", Config::config_path().display());
                anyhow::bail!("set EDITOR or VISUAL to open config.toml");
            };
            let mut parts = editor.split_whitespace();
            let program = parts.next().unwrap_or("notepad");
            let program = resolve_editor_program(program)?;
            let status = std::process::Command::new(program)
                .args(parts)
                .arg(Config::config_path())
                .status()
                .with_context(|| format!("launch editor `{editor}`"))?;
            anyhow::ensure!(status.success(), "editor exited with {status}");
        }
    }
    Ok(())
}

fn tool_inventory() -> Vec<(String, String)> {
    let schemas = varynth::tools::schemas();
    let mut rows: Vec<(String, String)> = Vec::new();
    for schema in &schemas {
        let Some(function) = schema.get("function") else {
            continue;
        };
        let Some(name) = function.get("name").and_then(|value| value.as_str()) else {
            continue;
        };
        let description = function
            .get("description")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        rows.push((name.to_string(), description));
    }
    rows
}

fn handle_tools(json: bool) -> Result<()> {
    let rows = tool_inventory();
    if json {
        let payload: Vec<serde_json::Value> = rows
            .iter()
            .map(
                |(name, description)| serde_json::json!({"name": name, "description": description}),
            )
            .collect();
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        for (name, description) in rows {
            println!("{name:<20} {description}");
        }
    }
    Ok(())
}

fn handle_theme(command: ThemeCommand, cfg: &mut Config) -> Result<()> {
    match command {
        ThemeCommand::List { json } => {
            let names = varynth::theme::choices();
            if json {
                println!("{}", serde_json::json!({"themes": names}));
            } else {
                for name in names {
                    println!("{name}");
                }
            }
        }
        ThemeCommand::Get { name, json } => {
            let name = name.unwrap_or_else(|| cfg.theme.clone());
            let rows = varynth::theme::role_rows(&name)?;
            if json {
                let mut map = serde_json::Map::new();
                for (key, value) in rows {
                    map.insert(key, serde_json::Value::String(value));
                }
                println!("{}", serde_json::Value::Object(map));
            } else {
                for (key, value) in rows {
                    println!("{key:<12} {value}");
                }
            }
        }
        ThemeCommand::Set { name, json } => {
            varynth::theme::ensure_valid(&name)?;
            cfg.theme = varynth::theme::canonical(&name);
            cfg.save()?;
            if json {
                println!("{}", serde_json::json!({"theme": cfg.theme}));
            } else {
                eprintln!("ok {}", cfg.theme);
            }
        }
        ThemeCommand::Preview { name } => preview_theme(&name)?,
        ThemeCommand::Custom(ThemeCustomCommand::Save { name }) => {
            let mut raw = String::new();
            io::Read::read_to_string(&mut io::stdin(), &mut raw)?;
            let value: serde_json::Value = serde_json::from_str(&raw).context("theme JSON")?;
            let path = varynth::theme::save_custom(&name, &value)?;
            println!("{}", path.display());
        }
        ThemeCommand::Custom(ThemeCustomCommand::Path { name }) => {
            println!("{}", varynth::theme::custom_path(&name)?.display());
        }
    }
    Ok(())
}

fn preview_theme(name: &str) -> Result<()> {
    varynth::theme::resolve(name)?;
    if !io::stdout().is_terminal() || std::env::var_os("NO_COLOR").is_some() {
        return Ok(());
    }
    use crossterm::execute;
    use crossterm::terminal::{
        disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
    };
    use ratatui::backend::CrosstermBackend;
    use ratatui::style::Style;
    use ratatui::text::{Line, Span};
    use ratatui::widgets::Paragraph;
    use ratatui::Terminal;
    use std::time::{Duration, Instant};
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(1) {
        terminal.draw(|frame| {
            varynth::theme::with_theme(name, || {
                let area = frame.area();
                frame.render_widget(
                    Paragraph::new(vec![
                        Line::from(Span::styled(
                            format!(" {name} "),
                            Style::default()
                                .fg(varynth::theme::Palette::bg())
                                .bg(varynth::theme::Palette::accent()),
                        )),
                        Line::from(Span::styled(
                            "ok theme",
                            Style::default().fg(varynth::theme::Palette::ok()),
                        )),
                    ])
                    .style(Style::default().bg(varynth::theme::Palette::bg())),
                    area,
                );
            });
        })?;
        std::thread::sleep(Duration::from_millis(50));
    }
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    Ok(())
}

fn handle_computer(command: ComputerCommand, cfg: &mut Config) -> Result<()> {
    use varynth::computer_use;
    match command {
        ComputerCommand::Status { json } => {
            if computer_use::detect_backend() == computer_use::Backend::Enigo {
                eprintln!("!! open-computer-use missing; using built-in enigo backend");
            }
            let value = computer_use::status_value(cfg.computer_use);
            if json {
                println!("{value}");
            } else {
                println!(
                    "enabled {}  backend {}",
                    value["enabled"],
                    value["backend"].as_str().unwrap_or("")
                );
            }
        }
        ComputerCommand::Doctor => {
            println!(
                "{}",
                serde_json::to_string_pretty(&computer_use::status_value(cfg.computer_use))?
            );
        }
        ComputerCommand::Enable => {
            cfg.computer_use = true;
            cfg.save()?;
            std::env::set_var("VARYNTH_COMPUTER_USE", "1");
            eprintln!("ok computer_use on");
        }
        ComputerCommand::Disable => {
            cfg.computer_use = false;
            cfg.save()?;
            std::env::remove_var("VARYNTH_COMPUTER_USE");
            eprintln!("ok computer_use off");
            eprintln!("flag: VARYNTH_COMPUTER_USE still wins if the parent shell exported it");
        }
        ComputerCommand::Install { force } => {
            let path = cfg
                .mcp_config
                .clone()
                .unwrap_or_else(|| Config::home_dir().join("mcp.json"));
            computer_use::install_ocu(&path, force)?;
            if cfg.mcp_config.is_none() {
                cfg.mcp_config = Some(path.clone());
                cfg.save()?;
            }
            eprintln!("ok {}", path.display());
        }
    }
    Ok(())
}

/// Decide which MCP registry this invocation may load. A path inside the
/// workspace is only honored when it was passed explicitly for this command
/// (`--mcp-config` / `VARYNTH_MCP_CONFIG`) or the workspace is trusted:
/// a persisted path from `config.toml` must never skip `varynth trust`.
fn select_mcp_config(
    cwd: &Path,
    configured: Option<PathBuf>,
    explicit: bool,
    trusted: bool,
) -> (Option<PathBuf>, Option<String>) {
    let untrusted_notice = |path: &Path| {
        format!(
            "!! ignoring {}: workspace is not trusted\n   run `varynth trust` to enable it, or pass --mcp-config explicitly",
            path.display()
        )
    };
    if let Some(path) = configured {
        let path = if path.is_absolute() {
            path
        } else {
            cwd.join(path)
        };
        if !explicit && !trusted && varynth::sandbox::path_within_workspace(cwd, &path) {
            return (None, Some(untrusted_notice(&path)));
        }
        return (Some(path), None);
    }
    let project = cwd.join(".varynth").join("mcp.json");
    if project.exists() {
        // In the user's home directory this path *is* the user-level
        // registry; it is the user's own config, not a clone's payload.
        if trusted || varynth::trust::is_user_home_workspace(cwd) {
            return (Some(project), None);
        }
        return (None, Some(untrusted_notice(&project)));
    }
    let home = Config::home_dir().join("mcp.json");
    if home.exists() {
        return (Some(home), None);
    }
    (None, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn cli_accepts_mcp_config_path() {
        let cli =
            Cli::try_parse_from(["varynth", "--mcp-config", "mcp.json", "exec", "hello"]).unwrap();
        assert_eq!(cli.mcp_config, Some(PathBuf::from("mcp.json")));
    }

    #[test]
    fn mcp_selection_never_trusts_a_persisted_workspace_path() {
        let workspace = tempfile::tempdir().unwrap();
        let cwd = workspace.path();
        let registry = cwd.join(".varynth").join("mcp.json");
        std::fs::create_dir_all(registry.parent().unwrap()).unwrap();
        std::fs::write(&registry, r#"{"mcpServers":{}}"#).unwrap();

        // A path that came from config.toml (not this invocation) is inert
        // for an untrusted workspace, even though it is inside the jail.
        let (selected, notice) = select_mcp_config(cwd, Some(registry.clone()), false, false);
        assert!(selected.is_none());
        assert!(notice.unwrap().contains("not trusted"));

        // The default project registry is subject to the same decision.
        let (selected, notice) = select_mcp_config(cwd, None, false, false);
        assert!(selected.is_none());
        assert!(notice.unwrap().contains("not trusted"));

        // An explicit `--mcp-config` for this command is the documented opt-in.
        let (selected, notice) = select_mcp_config(cwd, Some(registry.clone()), true, false);
        assert_eq!(selected, Some(registry.clone()));
        assert!(notice.is_none());

        // Trusted workspaces load it; revoking trust flips the same call.
        assert_eq!(
            select_mcp_config(cwd, None, false, true).0,
            Some(registry.clone())
        );
        assert!(select_mcp_config(cwd, Some(registry), false, false)
            .0
            .is_none());
    }

    #[test]
    fn editor_resolution_refuses_cwd_relative_programs() {
        assert!(resolve_editor_program("./evil.exe").is_err());
        assert!(resolve_editor_program("sub\\evil.exe").is_err());
        assert!(resolve_editor_program("sub/evil").is_err());
        let dir = tempfile::tempdir().unwrap();
        let editor = dir.path().join("editor.exe");
        std::fs::write(&editor, b"x").unwrap();
        assert_eq!(
            resolve_editor_program(&editor.display().to_string()).unwrap(),
            editor
        );
    }

    #[test]
    fn config_refuses_non_allowlisted_and_secret_keys() {
        let mut cfg = Config::default();
        for key in [
            "dashboard_token",
            "proxy_token",
            "openai_api_key",
            "telegram_bot_token",
            "add_dirs",
        ] {
            let error = set_config_value(&mut cfg, key, "x")
                .unwrap_err()
                .to_string();
            assert!(error.contains("varynth keyring"), "{key}: {error}");
        }
    }

    #[test]
    fn config_validates_percent_and_policy() {
        let mut cfg = Config::default();
        assert!(set_config_value(&mut cfg, "auto_compact_percent", "0").is_err());
        assert!(set_config_value(&mut cfg, "auto_compact_percent", "100").is_err());
        assert!(set_config_value(&mut cfg, "auto_compact_percent", "69").is_ok());
        assert_eq!(cfg.auto_compact_percent, 69);
        assert!(set_config_value(&mut cfg, "hooks_timeout_policy", "allow").is_err());
        assert!(set_config_value(&mut cfg, "dashboard_url", "x").is_err());
        assert!(set_config_value(&mut cfg, "decisions_url", "ftp://nope").is_err());
        assert!(set_config_value(
            &mut cfg,
            "decisions_url",
            "https://user:secret@127.0.0.1:8791"
        )
        .is_err());
        assert!(set_config_value(
            &mut cfg,
            "decisions_url",
            "https://127.0.0.1:8791?token=secret"
        )
        .is_err());
        assert!(set_config_value(&mut cfg, "permission_mode", "yolo").is_err());
    }

    #[test]
    fn tools_inventory_contains_bash_with_a_description() {
        let rows = tool_inventory();
        let bash = rows
            .iter()
            .find(|(name, _)| name == "bash")
            .expect("bash tool schema");
        assert!(!bash.1.is_empty());
        assert!(!bash.1.contains('\n'));
    }

    #[test]
    fn sandbox_docker_alias_normalizes() {
        assert_eq!(normalize_sandbox("docker").unwrap(), "docker-isolated");
        assert_eq!(
            normalize_sandbox("docker-isolated").unwrap(),
            "docker-isolated"
        );
        assert!(normalize_sandbox("nope").is_err());
    }

    #[test]
    fn output_modes_parse() {
        let cli =
            Cli::try_parse_from(["varynth", "exec", "hello", "--output", "stream-json"]).unwrap();
        match cli.command {
            Some(Commands::Exec { output, .. }) => assert_eq!(output, OutputMode::StreamJson),
            other => panic!("unexpected command: {other:?}"),
        }
    }
}
