use clap::{Args, Subcommand};

use super::{
  chat, index, init, inspect, install, list, list_tools, marketplace, run, server_ops, test,
  validate,
};

#[derive(Args)]
pub struct SkillArgs {
  #[command(subcommand)]
  command: SkillCommands,
}

#[derive(Subcommand)]
enum SkillCommands {
  /// Inspect or validate a local skill registry index
  Index(IndexArgs),
  /// Browse a local marketplace that groups skill registry indexes
  Marketplace(MarketplaceArgs),
  /// Create a new standard SKILL.md scaffold
  Init {
    /// Directory to create the skill in
    skill_dir: String,
    /// Skill name. Defaults to the target directory name.
    #[arg(long)]
    name: Option<String>,
    /// Skill description written to SKILL.md
    #[arg(short, long)]
    description: Option<String>,
    /// Overwrite scaffold files if they already exist
    #[arg(long)]
    force: bool,
  },
  /// Install a skill from a local registry index
  Install {
    /// Path to the skill registry index file
    index_file: String,
    /// Skill name or alias to install
    skill: String,
    /// Target skills directory (default: ~/.yanshi/skills)
    #[arg(short, long)]
    dir: Option<String>,
    /// Overwrite an existing installed skill directory
    #[arg(long)]
    force: bool,
  },
  /// Validate a skill manifest and print its configuration
  Validate {
    /// Path to the skill directory (must contain skill.toml or SKILL.md)
    skill_dir: String,
  },
  /// Inspect a skill manifest without running the agent
  Inspect {
    /// Path to the skill directory (must contain skill.toml or SKILL.md)
    skill_dir: String,
    /// Explain the capability decision for each declared tool
    #[arg(long = "explain-permissions")]
    explain_permissions: bool,
    /// Operator override: admit tool by name (repeatable). Beats skill manifest.
    #[arg(long = "allow-tool", value_name = "TOOL")]
    allow_tools: Vec<String>,
    /// Operator override: deny tool by name (repeatable). Highest precedence.
    #[arg(long = "deny-tool", value_name = "TOOL")]
    deny_tools: Vec<String>,
    /// Skip MCP capability discovery even when the manifest declares
    /// servers. P10.9.1 flipped the default: discovery is now on by
    /// default (cached in `~/.yanshi/cache/skill_mcp_discovery.json`
    /// for 24h, so repeat-inspects on the same skill are free).
    /// Pass this opt-out when you want to skip the cost entirely.
    #[arg(long = "no-mcp-discovery")]
    no_mcp_discovery: bool,
    /// Force a fresh MCP discovery and rewrite the cache entry.
    /// Use after upstream MCP servers ship a new tool advertisement
    /// and the 24h TTL hasn't expired yet.
    #[arg(long = "refresh-mcp-cache")]
    refresh_mcp_cache: bool,
    /// **Deprecated** (P10.9.1): MCP discovery is now the default
    /// when the manifest declares servers. The flag is kept as a
    /// no-op so existing scripts don't break; safe to remove.
    /// Use `--no-mcp-discovery` to opt out.
    #[arg(long = "with-mcp-discovery", hide = true)]
    with_mcp_discovery: bool,
  },
  /// Run a skill with a single message and exit.
  ///
  /// Local mode (default): treats the positional argument as a
  /// filesystem path to a skill directory, loads + validates the
  /// manifest, builds the agent in-process, and runs it.
  ///
  /// Server mode (`--server <url>` or `YANSHI_SERVER_URL`):
  /// treats the positional argument as a skill NAME resolved via
  /// the remote gateway's `YANSHI_SKILLS_INDEX` catalog, then
  /// dispatches via `POST /v1/skills/{name}:run` and polls until
  /// the run is terminal. `--memory`, `--model`, `--session`, and
  /// `--trace` are rejected in server mode because the wire
  /// contract doesn't accept per-request overrides today
  /// (P10.11.2 follow-up if needed).
  Run {
    /// Local mode: path to the skill directory. Server mode (with
    /// `--server`): skill name resolved via the server's catalog.
    skill_dir: String,
    /// The message to send to the agent. Optional when `--input` is
    /// given (the inputs are then the request).
    #[arg(short, long)]
    message: Option<String>,
    /// `KEY=VALUE` facts for the agent (e.g. `url=...`, `want=report`),
    /// appended to the message. **Local-only.**
    #[arg(long = "input", value_name = "KEY=VALUE")]
    inputs: Vec<String>,
    /// Copy the files produced by the skill's `[[workflows]]` tools to
    /// `<DIR>/<tool>/<node_id>/<file>`. **Local-only.**
    #[arg(long, value_name = "DIR")]
    output_dir: Option<String>,
    /// Run base directory for the skill's `[[workflows]]` tools (default:
    /// YANSHI_RUN_DIR or ~/.yanshi/runs). **Local-only.**
    #[arg(long, value_name = "DIR")]
    run_dir: Option<String>,
    /// Override the model declared by the skill manifest.
    /// **Local-only** — incompatible with `--server`.
    #[arg(long)]
    model: Option<String>,
    /// Override memory backend for this run: session, sqlite, or none.
    /// **Local-only** — incompatible with `--server`.
    #[arg(long, value_parser = ["session", "sqlite", "none"])]
    memory: Option<String>,
    /// Reuse an existing session ID for multi-turn conversations.
    /// **Local-only** — incompatible with `--server`.
    #[arg(long, visible_alias = "session-id")]
    session: Option<String>,
    /// Print the structured AgentRuntime trace as JSON (text mode only;
    /// in `--output json` mode the trace is inlined under the `trace`
    /// key of the response payload instead). **Local-only** — server
    /// runs emit their trace through the event log; consume it via
    /// `yanshi workflow logs <run_id>`.
    #[arg(long)]
    trace: bool,
    /// Output format. Local mode: `text` (default — emoji banner +
    /// `🤖 Agent:`) or `json` (single JSON object). Server mode:
    /// `text` (final run row pretty-printed) or `json-envelope`
    /// (canonical `CliJsonEnvelope` wrapping the run row;
    /// progress goes to stderr). Warnings always go to stderr.
    #[arg(long, default_value = "text", value_parser = ["text", "json", "json-envelope"])]
    output: String,
    /// Dispatch the run to a remote `yanshi serve` instance
    /// instead of running in-process. Falls back to
    /// YANSHI_SERVER_URL when omitted; when neither is set the
    /// CLI runs the skill locally. In server mode the positional
    /// argument is the skill NAME, not a filesystem path.
    #[arg(long)]
    server: Option<String>,
    /// Bearer token for the remote server (also YANSHI_API_TOKEN).
    /// Only consulted when --server is set.
    #[arg(long)]
    auth_token: Option<String>,
    /// Tenant id scope for server-mode requests. Defaults to
    /// YANSHI_TENANT or "default".
    #[arg(long)]
    tenant: Option<String>,
  },
  /// Start an interactive multi-turn chat session with a skill
  Chat {
    /// Path to the skill directory
    skill_dir: String,
    /// Override the model declared by the skill manifest
    #[arg(long)]
    model: Option<String>,
    /// Override memory backend for this chat: session, sqlite, or none
    #[arg(long, value_parser = ["session", "sqlite", "none"])]
    memory: Option<String>,
    /// Resume an existing session by ID (optional)
    #[arg(long, visible_alias = "session-id")]
    session: Option<String>,
  },
  /// List available skills in a directory
  List {
    /// Skills directory (default: ~/.yanshi/skills)
    #[arg(short, long)]
    dir: Option<String>,
  },
  /// List built-in, script, and MCP tools exposed by a skill
  ListTools {
    /// Path to the skill directory
    skill_dir: String,
  },
  /// Run skill validation, tool discovery, and minimal regression checks
  Test {
    /// Path to the skill directory
    skill_dir: String,
    /// Only validate manifest and discover tools; do not execute regressions or smoke scripts
    #[arg(long)]
    dry_run: bool,
    /// Also run tests/smoke.sh when present
    #[arg(long)]
    smoke: bool,
  },
}

#[derive(Args)]
struct IndexArgs {
  #[command(subcommand)]
  command: SkillIndexCommands,
}

#[derive(Args)]
struct MarketplaceArgs {
  #[command(subcommand)]
  command: SkillMarketplaceCommands,
}

#[derive(Subcommand)]
enum SkillIndexCommands {
  Validate {
    /// Path to the skill registry index file
    index_file: String,
  },
  List {
    /// Path to the skill registry index file
    index_file: String,
  },
  Resolve {
    /// Path to the skill registry index file
    index_file: String,
    /// Skill name or alias to resolve
    skill: String,
  },
}

#[derive(Subcommand)]
enum SkillMarketplaceCommands {
  Validate {
    /// Path to the skill marketplace file
    marketplace_file: String,
  },
  List {
    /// Path to the skill marketplace file
    marketplace_file: String,
  },
  Resolve {
    /// Path to the skill marketplace file
    marketplace_file: String,
    /// Skill name or alias to resolve
    skill: String,
  },
  Install {
    /// Path to the skill marketplace file
    marketplace_file: String,
    /// Skill name or alias to install
    skill: String,
    /// Target skills directory (default: ~/.yanshi/skills)
    #[arg(short, long)]
    dir: Option<String>,
    /// Overwrite an existing installed skill directory
    #[arg(long)]
    force: bool,
  },
}

pub async fn dispatch(args: SkillArgs) -> anyhow::Result<()> {
  match args.command {
    SkillCommands::Index(args) => match args.command {
      SkillIndexCommands::Validate { index_file } => index::validate(index_file).await,
      SkillIndexCommands::List { index_file } => index::list(index_file).await,
      SkillIndexCommands::Resolve { index_file, skill } => index::resolve(index_file, skill).await,
    },
    SkillCommands::Marketplace(args) => match args.command {
      SkillMarketplaceCommands::Validate { marketplace_file } => {
        marketplace::validate(marketplace_file).await
      }
      SkillMarketplaceCommands::List { marketplace_file } => {
        marketplace::list(marketplace_file).await
      }
      SkillMarketplaceCommands::Resolve {
        marketplace_file,
        skill,
      } => marketplace::resolve(marketplace_file, skill).await,
      SkillMarketplaceCommands::Install {
        marketplace_file,
        skill,
        dir,
        force,
      } => marketplace::install(marketplace_file, skill, dir, force).await,
    },
    SkillCommands::Init {
      skill_dir,
      name,
      description,
      force,
    } => init::execute(skill_dir, name, description, force).await,
    SkillCommands::Install {
      index_file,
      skill,
      dir,
      force,
    } => install::execute(index_file, skill, dir, force).await,
    SkillCommands::Validate { skill_dir } => validate::execute(skill_dir).await,
    SkillCommands::Inspect {
      skill_dir,
      explain_permissions,
      allow_tools,
      deny_tools,
      no_mcp_discovery,
      refresh_mcp_cache,
      with_mcp_discovery,
    } => {
      inspect::execute(
        skill_dir,
        explain_permissions,
        allow_tools,
        deny_tools,
        no_mcp_discovery,
        refresh_mcp_cache,
        with_mcp_discovery,
      )
      .await
    }
    SkillCommands::Run {
      skill_dir,
      message,
      inputs,
      output_dir,
      run_dir,
      model,
      memory,
      session,
      trace,
      output,
      server,
      auth_token,
      tenant,
    } => match crate::server_client::resolve_server_url(server.as_deref()) {
      Some(server_url) => {
        // Reject local-only flags (including the local-only `json`
        // output value) BEFORE any HTTP call.
        let validation = server_ops::reject_local_only_flags(
          model.as_deref(),
          memory.as_deref(),
          session.as_deref(),
          trace,
          &output,
        );
        let validation = validation.and_then(|()| {
          if !inputs.is_empty() || output_dir.is_some() || run_dir.is_some() {
            anyhow::bail!(
              "--input / --output-dir / --run-dir are local-only; drop them with --server"
            )
          }
          message
            .clone()
            .ok_or_else(|| anyhow::anyhow!("--message is required with --server"))
        });
        match validation {
          Ok(message) => {
            server_ops::run_via_server(
              &server_url,
              auth_token.as_deref(),
              tenant.as_deref(),
              &skill_dir,
              &message,
              &output,
            )
            .await
          }
          Err(err) => Err(err),
        }
      }
      None => {
        // Local mode: server-only flags being set is a soft
        // misconfiguration — the user probably intended to set
        // YANSHI_SERVER_URL too. Warn but don't bail; local
        // mode is the fallback.
        if auth_token.is_some() || tenant.is_some() {
          eprintln!(
            "⚠  --auth-token / --tenant ignored: --server (or YANSHI_SERVER_URL) is not set"
          );
        }
        run::execute(run::RunArgs {
          skill_dir,
          message,
          inputs,
          output_dir,
          run_dir,
          model_override: model,
          memory_override: memory,
          session_id: session,
          trace,
          output,
        })
        .await
      }
    },
    SkillCommands::Chat {
      skill_dir,
      model,
      memory,
      session,
    } => chat::execute(skill_dir, model, memory, session).await,
    SkillCommands::List { dir } => list::execute(dir).await,
    SkillCommands::ListTools { skill_dir } => list_tools::execute(skill_dir).await,
    SkillCommands::Test {
      skill_dir,
      dry_run,
      smoke,
    } => test::execute(skill_dir, dry_run, smoke).await,
  }
}
