use clap::{Args, Subcommand};

use super::{debug, dynamic, resume_plan, run, server_ops, validate};

#[derive(Args)]
pub struct WorkflowArgs {
  #[command(subcommand)]
  command: WorkflowCommands,
}

#[derive(Subcommand)]
enum WorkflowCommands {
  Run {
    workflow_file: String,
    #[arg(short, long)]
    watch: bool,
    #[arg(short, long)]
    output: Option<String>,
    /// Override the model used by LLM nodes in this workflow
    #[arg(short = 'm', long)]
    model: Option<String>,
    #[arg(short, long, num_args = 2, value_names = ["KEY", "VALUE"])]
    input: Vec<String>,
    #[arg(long)]
    dry_run: bool,
    #[arg(long, default_value = "60s")]
    timeout: String,
    #[arg(long, default_value_t = 0)]
    max_retries: u32,
    /// Workflow execution mode: serial or concurrent
    #[arg(long, default_value = "serial", value_parser = ["serial", "concurrent"])]
    execution_mode: String,
    /// Maximum concurrently running workflow nodes when --execution-mode concurrent
    #[arg(long, default_value_t = 4)]
    max_concurrency: usize,
    /// Base directory for per-run workflow artifacts. Defaults to YANSHI_RUN_DIR or ~/.yanshi/runs.
    #[arg(long)]
    run_dir: Option<String>,
    /// Copy every artifact the run produced to `<DIR>/<node_id>/<file>`
    /// and write `<DIR>/artifacts.json` (size + SHA-256 per file).
    #[arg(long, value_name = "DIR")]
    output_dir: Option<String>,
    /// Submit the workflow to a remote `yanshi serve` instance instead
    /// of executing in-process. Falls back to YANSHI_SERVER_URL when
    /// the flag is omitted; if neither is set the CLI runs in-process.
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
    /// Output format when running via `--server`: text (default;
    /// emoji progress + final JSON row on stdout) or json-envelope
    /// (canonical `CliJsonEnvelope` — `yanshi.cli/1` wire schema;
    /// progress lines go to stderr so stdout stays parseable JSON).
    /// Ignored for in-process runs.
    #[arg(long, default_value = "text", value_parser = ["text", "json-envelope"])]
    format: String,
  },
  /// List recent workflow runs from a remote server. Requires --server.
  List {
    #[arg(long)]
    server: Option<String>,
    #[arg(long)]
    auth_token: Option<String>,
    #[arg(long)]
    tenant: Option<String>,
    #[arg(long)]
    limit: Option<i64>,
    #[arg(long)]
    offset: Option<i64>,
    #[arg(long)]
    status: Option<String>,
    /// Output format: json (default — legacy bare body) or
    /// json-envelope (canonical `CliJsonEnvelope` —
    /// `yanshi.cli/1` wire schema)
    #[arg(long, default_value = "json", value_parser = ["json", "json-envelope"])]
    format: String,
  },
  /// Cancel a queued / running workflow run on a remote server.
  Cancel {
    run_id: String,
    #[arg(long)]
    server: Option<String>,
    #[arg(long)]
    auth_token: Option<String>,
    #[arg(long)]
    tenant: Option<String>,
    /// Output format: json (default — legacy bare body) or
    /// json-envelope (canonical `CliJsonEnvelope`)
    #[arg(long, default_value = "json", value_parser = ["json", "json-envelope"])]
    format: String,
  },
  /// Stream a workflow run's event log from a remote server.
  ///
  /// Without `--follow` (default), fetches the already-persisted
  /// events as a single JSON array via
  /// `GET /v1/runs/{id}/events/history`. With `--follow`, opens an
  /// SSE connection at `GET /v1/runs/{id}/events` and keeps
  /// streaming until the server closes or the user cancels.
  ///
  /// Reconnecting consumers can pass `--after-seq <n>` to resume
  /// past the last `seq` they printed, avoiding duplicates.
  Logs {
    /// Run id whose event log should be streamed.
    run_id: String,
    #[arg(long)]
    server: Option<String>,
    #[arg(long)]
    auth_token: Option<String>,
    #[arg(long)]
    tenant: Option<String>,
    /// Keep the connection open and stream events as they're
    /// emitted (SSE). Default: false (history-only snapshot).
    #[arg(short = 'f', long, default_value_t = false)]
    follow: bool,
    /// Resume after this `seq`. Useful for reconnecting consumers
    /// to avoid duplicate output.
    #[arg(long)]
    after_seq: Option<i64>,
    /// Output format: text (default — human-readable one line per
    /// event), json (JSONL — one event JSON per line), or
    /// json-envelope (single canonical `CliJsonEnvelope` wrapping
    /// the events array; incompatible with `--follow` because an
    /// envelope is bounded and a follow stream is not).
    #[arg(long, default_value = "text", value_parser = ["text", "json", "json-envelope"])]
    format: String,
  },
  /// Validate workflow schema and dependencies without execution
  Validate {
    workflow_file: String,
    /// Output format: text, json (legacy bare body), or json-envelope
    /// (canonical `CliJsonEnvelope` — `yanshi.cli/1` wire schema)
    #[arg(long, default_value = "text", value_parser = ["text", "json", "json-envelope"])]
    format: String,
    /// Treat schema warnings for unknown node parameters as validation errors
    #[arg(long)]
    strict: bool,
    /// Print the per-node permission and capability requirements
    #[arg(long = "explain-permissions")]
    explain_permissions: bool,
  },
  /// Inspect the resume plan for a checkpointed workflow run
  ResumePlan {
    /// Run / workflow id whose checkpoint should be inspected
    run_id: String,
    /// Checkpoint directory (default: ~/.yanshi/checkpoints)
    #[arg(long)]
    checkpoint_dir: Option<String>,
    /// Treat `Unknown` idempotency calls as safe to replay
    #[arg(long)]
    force_replay: bool,
    /// Output format: text, json (legacy bare body), or json-envelope
    /// (canonical `CliJsonEnvelope` — `yanshi.cli/1` wire schema)
    #[arg(long, default_value = "text", value_parser = ["text", "json", "json-envelope"])]
    format: String,
  },
  /// Debug and inspect workflow structure
  Debug {
    workflow_file: String,
    /// Visualize the workflow DAG
    #[arg(long)]
    visualize: bool,
    /// Perform dry run without execution
    #[arg(long)]
    dry_run: bool,
    /// Analyze workflow structure and dependencies
    #[arg(long)]
    analyze: bool,
    /// Validate workflow configuration
    #[arg(long)]
    validate: bool,
    /// Show execution plan
    #[arg(long)]
    plan: bool,
    /// Enable verbose output
    #[arg(short, long)]
    verbose: bool,
  },
  /// Run a dynamic workflow: an LLM authors a declarative plan for the goal,
  /// which is compiled to a Flow and executed (in parallel where the plan
  /// allows). The plan is LLM-authored then executed, so tool access is
  /// governed: built-in tools are sandboxed (file paths / HTTP domains must
  /// be granted via --allow-path / --allow-domain; shell is never available),
  /// modality tools (tts/asr/image/video generation) are off unless
  /// --allow-modalities is passed, --dry-run prints the plan without
  /// running it, and --approve routes every call through the Harness
  /// approval pipeline.
  Dynamic {
    /// Natural-language goal the LLM plans a workflow for.
    #[arg(long)]
    goal: String,
    /// Model the LLM planner uses to author the plan. Required.
    #[arg(short = 'm', long)]
    model: Option<String>,
    /// Grant the built-in file tool access to a path (repeatable).
    #[arg(long = "allow-path")]
    allow_path: Vec<String>,
    /// Grant the built-in HTTP tool access to a domain (repeatable).
    #[arg(long = "allow-domain")]
    allow_domain: Vec<String>,
    /// Register the modality Tool adapters (tts, asr, text_to_image,
    /// image_to_image, image_edit, image_understand, text_to_video) so the
    /// LLM-authored plan can call them — driven entirely by the model
    /// registry YAML, same as `--allow-path`/`--allow-domain` for
    /// file/HTTP access. Off by default: these are billed calls to a
    /// vendor API, so (like shell) they're opt-in rather than always
    /// available to an adversarial-by-construction plan.
    #[arg(long = "allow-modalities")]
    allow_modalities: bool,
    /// Approval pipeline for tool calls: none, cli (interactive),
    /// auto-allow, or auto-deny. Unset defaults to `cli` under
    /// `local`/`production` `--profile` (an LLM-authored plan is
    /// adversarial by construction) and to `none` under `dev` (so local
    /// iteration stays uninterrupted). Pass `--approve none` explicitly
    /// to run non-`dev` unsupervised — T1.3.
    #[arg(long, value_parser = ["none", "cli", "auto-allow", "auto-deny"])]
    approve: Option<String>,
    /// Security profile driving approval escalation (dev | production).
    /// Defaults to `dev`, which (per the `--approve` default above) runs
    /// tool calls unsupervised. Unlike `harness run`/`chat` (default
    /// `local`, interactive approval), invoking `workflow dynamic` with
    /// no flags at all does NOT prompt for approval —
    /// pass `--profile local` or `--profile production` explicitly for
    /// CI/production use (U4.1: kept as-is intentionally, so existing
    /// unsupervised-iteration scripts keep working; see
    /// docs/HYBRID_WORKFLOW.md).
    #[arg(long, default_value = "dev")]
    profile: String,
    /// Print the authored plan and exit without executing any tool.
    #[arg(long)]
    dry_run: bool,
    /// Maximum concurrently running steps.
    #[arg(long, default_value_t = 8)]
    max_concurrency: usize,
    /// Recover from step failures by asking the planner for a revised
    /// plan covering only what's still missing, up to this many rounds
    /// (0, the default, disables replanning — a failed step fails the
    /// run, same as before this flag existed). Already-succeeded steps
    /// are never re-run. Every replan round's new tool calls go through
    /// the same governed registry (and --approve pipeline, when set) as
    /// the first round — no separate re-approval step is needed. Has no
    /// effect combined with --dry-run, since nothing executes there for
    /// a step to fail and trigger a revision.
    #[arg(long, default_value_t = 0)]
    replan: usize,
    /// Output format: text (default) or json.
    #[arg(long, default_value = "text", value_parser = ["text", "json"])]
    output: String,
  },
}

pub async fn dispatch(args: WorkflowArgs) -> anyhow::Result<()> {
  match args.command {
    WorkflowCommands::Run {
      workflow_file,
      watch,
      output,
      model,
      input,
      dry_run,
      timeout,
      max_retries,
      execution_mode,
      max_concurrency,
      run_dir,
      output_dir,
      server,
      auth_token,
      tenant,
      format,
    } => {
      if input.len() % 2 != 0 {
        eprintln!(
          "Error: Input must be provided in key-value pairs. Got {} arguments (expected even number).",
          input.len()
        );
        std::process::exit(1);
      }
      // Server-mode short-circuits the in-process executor when
      // --server (or YANSHI_SERVER_URL) is set. P10.11.4
      // closed the silent-drop class of bug: every per-run knob
      // the local executor consumes is now rejected up front
      // with an actionable message naming the local mode
      // alternative or future-API status.
      if let Some(server_url) = crate::server_client::resolve_server_url(server.as_deref()) {
        // Defaults must match the clap flag definitions above —
        // the validator only fires when the operator explicitly
        // overrode them.
        const EXECUTION_MODE_DEFAULT: &str = "serial";
        const MAX_CONCURRENCY_DEFAULT: usize = 4;
        const TIMEOUT_DEFAULT: &str = "60s";
        let validation = server_ops::reject_local_only_flags(
          model.as_deref(),
          &execution_mode,
          EXECUTION_MODE_DEFAULT,
          max_concurrency,
          MAX_CONCURRENCY_DEFAULT,
          run_dir.as_deref(),
          watch,
          output.as_deref(),
          output_dir.as_deref(),
          &input,
          dry_run,
          &timeout,
          TIMEOUT_DEFAULT,
          max_retries,
        );
        match validation {
          Err(err) => Err(err),
          Ok(()) => match std::fs::read_to_string(&workflow_file) {
            Ok(body) => {
              server_ops::run_via_server(
                &server_url,
                auth_token.as_deref(),
                tenant.as_deref(),
                &body,
                &format,
              )
              .await
            }
            Err(e) => Err(anyhow::anyhow!(
              "failed to read workflow file '{workflow_file}': {e}"
            )),
          },
        }
      } else {
        let input_pairs = input
          .as_chunks::<2>()
          .0
          .iter()
          .map(|[key, value]| (key.clone(), value.clone()))
          .collect();
        run::execute(
          workflow_file,
          watch,
          output,
          model,
          input_pairs,
          dry_run,
          timeout,
          max_retries,
          execution_mode,
          max_concurrency,
          run_dir,
          output_dir,
        )
        .await
      }
    }
    WorkflowCommands::List {
      server,
      auth_token,
      tenant,
      limit,
      offset,
      status,
      format,
    } => match crate::server_client::resolve_server_url(server.as_deref()) {
      Some(server_url) => {
        server_ops::list(
          &server_url,
          auth_token.as_deref(),
          tenant.as_deref(),
          limit,
          offset,
          status.as_deref(),
          &format,
        )
        .await
      }
      None => Err(anyhow::anyhow!(
        "workflow list requires --server <url> or YANSHI_SERVER_URL to be set"
      )),
    },
    WorkflowCommands::Cancel {
      run_id,
      server,
      auth_token,
      tenant,
      format,
    } => match crate::server_client::resolve_server_url(server.as_deref()) {
      Some(server_url) => {
        server_ops::cancel(
          &server_url,
          auth_token.as_deref(),
          tenant.as_deref(),
          &run_id,
          &format,
        )
        .await
      }
      None => Err(anyhow::anyhow!(
        "workflow cancel requires --server <url> or YANSHI_SERVER_URL to be set"
      )),
    },
    WorkflowCommands::Logs {
      run_id,
      server,
      auth_token,
      tenant,
      follow,
      after_seq,
      format,
    } => match crate::server_client::resolve_server_url(server.as_deref()) {
      Some(server_url) => {
        server_ops::logs(
          &server_url,
          auth_token.as_deref(),
          tenant.as_deref(),
          &run_id,
          follow,
          after_seq,
          &format,
        )
        .await
      }
      None => Err(anyhow::anyhow!(
        "workflow logs requires --server <url> or YANSHI_SERVER_URL to be set"
      )),
    },
    WorkflowCommands::Validate {
      workflow_file,
      format,
      strict,
      explain_permissions,
    } => validate::execute(workflow_file, format, strict, explain_permissions).await,
    WorkflowCommands::ResumePlan {
      run_id,
      checkpoint_dir,
      force_replay,
      format,
    } => resume_plan::execute(run_id, checkpoint_dir, force_replay, format).await,
    WorkflowCommands::Debug {
      workflow_file,
      visualize,
      dry_run,
      analyze,
      validate,
      plan,
      verbose,
    } => {
      debug::execute(
        workflow_file,
        visualize,
        dry_run,
        analyze,
        validate,
        plan,
        verbose,
      )
      .await
    }
    WorkflowCommands::Dynamic {
      goal,
      model,
      allow_path,
      allow_domain,
      allow_modalities,
      approve,
      profile,
      dry_run,
      max_concurrency,
      replan,
      output,
    } => {
      dynamic::execute(
        goal,
        model,
        allow_path,
        allow_domain,
        allow_modalities,
        approve,
        profile,
        dry_run,
        max_concurrency,
        replan,
        output,
      )
      .await
    }
  }
}
