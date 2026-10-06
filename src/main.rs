use anyhow::{Context, Result};
use chrono::{Duration, Utc};
use clap::{Parser, Subcommand, ValueEnum};
use rustyline::DefaultEditor;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use varynth::automation::{parse_trigger_with_cron, TaskStore, Trigger};
use varynth::config::{
    keyring_delete, keyring_get, keyring_set, normalize_keyring_field, Config, KEYRING_FIELDS,
};
use varynth::runtime::Runtime;
use varynth::session::Session;
use varynth::{channels, dashboard, doctor, install, mcp, plugins, skills_search, tui};

#[derive(Parser, Debug)]
#[command(name = "varynth", version, about = "Varynth local coding agent")]
struct Cli {
    /// Working directory
    #[arg(short = 'C', long)]
    cwd: Option<PathBuf>,
    /// Model id (provider:name on proxy)
    #[arg(short, long)]
    model: Option<String>,
    /// Provider: proxy | anthropic | openai
    #[arg(long)]
    provider: Option<String>,
    /// Permission mode: acceptEdits | prompt | bypass
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
    /// List saved sessions
    Sessions,
    /// List models from the active provider
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
    /// Manage model-driven reminders and automation tasks
    Task {
        #[command(subcommand)]
        command: TaskCommand,
    },
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
enum McpCommand {
    Status,
    Probe { name: String },
    Ping { name: String },
}

#[derive(Clone, Copy, Debug, ValueEnum, PartialEq, Eq)]
enum OutputMode {
    Text,
    Json,
    StreamJson,
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

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "varynth=warn,tower_http=warn".into()),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();
    Config::ensure_home()?;
    let mut cfg = Config::load()?;
    if let Some(m) = cli.model {
        cfg.model = m;
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
    if let Some(path) = cfg.mcp_config.take() {
        cfg.mcp_config = Some(if path.is_absolute() {
            path
        } else {
            cwd.join(path)
        });
    } else {
        let project_mcp = cwd.join(".varynth").join("mcp.json");
        if project_mcp.exists() {
            cfg.mcp_config = Some(project_mcp);
        }
    }
    cfg.validate()?;

    match cli.command.unwrap_or(Commands::Repl) {
        Commands::Init => {
            cfg.save()?;
            println!("wrote {}", Config::config_path().display());
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
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        Commands::Keyring { command } => handle_keyring(command)?,
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
        Commands::Models => {
            let rt = Runtime::new(cfg, cwd)?;
            let models = rt.list_models().await?;
            for m in models {
                println!("{}", m.id);
            }
        }
        Commands::Sessions => {
            for s in Session::list()? {
                println!(
                    "{}  {}  {}",
                    s.id,
                    s.updated_at.format("%Y-%m-%d %H:%M"),
                    s.title
                );
            }
        }
        Commands::Channels => {
            println!("{}", serde_json::to_string_pretty(&channels::v2_status())?);
        }
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
        Commands::Repl => {
            let rt = Runtime::new(cfg.clone(), cwd.clone())?;
            let session = Session::new(&cwd.display().to_string(), &cfg.model)?;
            start_ui(rt, session, cli.plain).await?;
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

fn handle_mcp(command: McpCommand, configured: Option<&Path>) -> Result<()> {
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
    println!("slash: /status /models /skills /review /goal /send /inbox /remind /serve /quit   skill: /name");
    let mut rl = DefaultEditor::new()?;
    loop {
        let line = match rl.readline("varynth › ") {
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
                                eprintln!("  ▸ {}", ev.text);
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
