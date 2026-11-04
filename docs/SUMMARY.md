# Summary

<!--
  Table of contents for the Yanshi manual (built by `mdbook build site`,
  published to GitHub Pages by .github/workflows/docs.yml). Only files listed
  here are rendered; links from a chapter to anything else (source files,
  archive/, dated evaluations) are rewritten to GitHub URLs by
  site/repo_links.py, which also fails the build on a link to a missing file.
-->

[Introduction](INTRODUCTION.md)

# Getting Started

- [Configuration](CONFIGURATION.md)
- [Trial Guide (中文)](TRIAL_GUIDE.md)
- [CLI Config-First Tutorial](examples/cli_config_first_tutorial.md)
- [Runnable Tutorials (中文)](examples/runnable_tutorials_zh.md)
- [Example Index](examples/README.md)
  - [Simple Agent LLM Flow](examples/simple_agent_llm_flow.md)
  - [Rust Interview Code-First Workflow (中文)](examples/rust_interview_code_first_proper_test_zh.md)

# Workflows

- [Workflow Schema](WORKFLOW_SCHEMA.md)
- [Expression Language](EXPRESSION_LANGUAGE.md)
- [Hybrid Workflow](HYBRID_WORKFLOW.md)
- [Workflow Debugging](WORKFLOW_DEBUGGING.md)
- [Checkpoint Recovery](CHECKPOINT_RECOVERY.md)
  - [Checkpoint Schema](CHECKPOINT_SCHEMA.md)
- [Retry Mechanism](RETRY_MECHANISM.md)
- [Timeout Control](TIMEOUT_CONTROL.md)

# Agents

- [Agent Runtime](AGENT_RUNTIME.md)
- [Multi-Agent Collaboration](MULTI_AGENT.md)
- [Memory Layering](MEMORY_LAYERING.md)
- [Harness Mode](HARNESS_MODE.md)
- [Agent Eval Format](AGENT_EVAL_FORMAT.md)

# Skills, Tools and Plugins

- [Skills](SKILLS.md)
  - [Skill Format](SKILL_FORMAT.md)
  - [Skill Registry](SKILL_REGISTRY.md)
  - [Skill Validator Protocol](SKILL_VALIDATOR_PROTOCOL.md)
- [Tool Permissions](TOOL_PERMISSIONS.md)
  - [Skill / Tool / CLI Permission Merge](SKILL_PERMISSIONS.md)
  - [Security Profiles](SECURITY_PROFILES.md)
- [Marketplace](MARKETPLACE.md)
- [Plugins](PLUGIN_DESIGN.md)

# MCP

- [MCP Skills](MCP_SKILLS.md)
- [MCP Capability Policy](MCP_CAPABILITY_POLICY.md)
- [MCP Testing and Examples](MCP_TEST_EXAMPLES_GUIDE.md)

# Models, Multimodal and RAG

- [LLM Providers Matrix](LLM_PROVIDERS_MATRIX.md)
- [Granular Model Types](GRANULAR_MODEL_TYPES.md)
- [Multimodal Guide](MULTIMODAL_GUIDE.md)
- [RAG Evaluation](RAG_EVAL.md)

# Observability

- [Tracing Usage](TRACING_USAGE.md)
  - [Trace Persistence Schema](TRACE_PERSISTENCE_SCHEMA.md)
- [CLI JSON Output](CLI_JSON_OUTPUT.md)
- [Web UI](WEB_UI.md)

# Operations

- [Deployment](DEPLOYMENT.md)
  - [Kubernetes](KUBERNETES_DEPLOYMENT.md)
  - [Health Checks](HEALTH_CHECKS.md)
- [Distributed Scheduling](DISTRIBUTED.md)
- [Secret Management](SECRET_MANAGEMENT.md)
- [Database Migrations](MIGRATIONS.md)
- [Backup and Restore](SERVER_BACKUP_RESTORE.md)
- [Operations Handbook (中文)](OPERATIONS_HANDBOOK.md)

# Extending Yanshi

- [Agent SDK](AGENT_SDK.md)
- [Extensibility Model](EXTENSIBILITY_MODEL.md)

# Reference

- [Stability Boundaries](STABILITY.md)
- [API Compatibility](API_COMPATIBILITY.md)
- [Migration Guide v0.1 → v0.2](MIGRATION_GUIDE_v0.2.0.md)
- [Release Notes v1.0.0-rc.1](RELEASE_NOTES_v1.0.0-rc.1.md)
- [Release Notes v0.2.0](RELEASE_NOTES_v0.2.0.md)

# Design and Internals

- [Architecture](ARCHITECTURE.md)
  - [Architecture Diagram (中文)](ARCHITECTURE_DIAGRAM.md)
- [Current Status](CURRENT_STATUS.md)
- [Evaluations]()
  - [Project Evaluation 2026-11-04](PROJECT_EVALUATION_2026-11-04.md)
  - [Project Evaluation 2026-10-07](PROJECT_EVALUATION_2026-10-07.md)
  - [Project Evaluation 2026-08-09](PROJECT_EVALUATION_2026-08-09.md)
  - [Project Evaluation 2026-08-05](PROJECT_EVALUATION_2026-08-05.md)
  - [Architecture Evaluation 2026-06-20](ARCHITECTURE_EVALUATION_2026-06-20.md)
- [Roadmap v2](ROADMAP_v2.md)
- [Tracing Design (中文)](TRACING_DESIGN.md)
- [MCP Production Design](MCP_PRODUCTION_DESIGN.md)
- [MCP and Skills Integration (中文)](MCP_SKILLS_INTEGRATION.md)
- [CI Workflows](CI_WORKFLOWS.md)
- [Release Checklist](RELEASE_CHECKLIST.md)
- [Promotion Criteria]()
  - [LLM Provider Modules](LLM_PROVIDER_MODULE_PROMOTION.md)
  - [Harness H6](H6_PROMOTION_CRITERIA.md)
- [RFCs]()
  - [Content Studio Runtime](RFC_CONTENT_STUDIO.md)
  - [Crate Architecture](RFC_CRATE_ARCHITECTURE.md)
  - [Nodes Decomposition](RFC_NODES_DECOMPOSITION.md)
  - [Tool Contract Split](RFC_TOOL_CONTRACT_SPLIT.md)
  - [Tool Distribution](RFC_TOOL_DISTRIBUTION.md)
  - [Harness Loop Ownership](RFC_HARNESS_LOOP_OWNERSHIP.md)
  - [Cross-Session Memory Linking](RFC_CROSS_SESSION_MEMORY_LINKING.md)
  - [Code-Execution Trust](RFC_CODE_EXECUTION_TRUST.md)
  - [LLM Code Execution](RFC_LLM_CODE_EXECUTION.md)
  - [MCP Protocol Modernization](RFC_MCP_PROTOCOL_MODERNIZATION.md)
  - [WASM Plugin Evaluation](WASM_PLUGIN_EVALUATION.md)
