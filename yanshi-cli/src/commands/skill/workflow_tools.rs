//! Y4.4: the files a skill's workflow tools produced. The tools
//! themselves are built by `yanshi_config::skill_workflows` (shared with
//! the gateway, Y8.5).

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
pub use yanshi_config::skill_workflows::{RecordedRun, WorkflowTools, build};

/// Copy each recorded run's artifacts to `<out>/<tool>/<node_id>/<file>`
/// (`<tool>-2`, `-3`, ... when a tool ran more than once) with a
/// manifest per run. Returns the number of files copied.
pub fn export(runs: Vec<RecordedRun>, run_base_dir: &Path, out: &Path) -> Result<usize> {
  let mut seen: HashMap<String, usize> = HashMap::new();
  let mut copied = 0;
  for run in runs {
    let count = seen.entry(run.tool.clone()).or_insert(0);
    *count += 1;
    let dir = if *count == 1 {
      out.join(&run.tool)
    } else {
      out.join(format!("{}-{count}", run.tool))
    };
    let files = yanshi_core::run_context::collect_file_outputs(&run.state, run_base_dir);
    copied +=
      crate::commands::workflow::artifacts::copy_to_output_dir(&files, &dir, &run.run_id)?.len();
  }
  Ok(copied)
}
