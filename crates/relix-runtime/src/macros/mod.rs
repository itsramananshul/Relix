//! The **Macro** (Pillar 1, native execute_code).
//!
//! A Macro lets an Operative collapse an N-step chain into *one*
//! call: it writes a single script that does all the work, the
//! script runs once, and only its `stdout` comes back — instead of
//! N separate tool round-trips, each paying a full inference turn.
//! For mechanical glue (filter, loop, reduce a large output) this
//! is the cheapest primitive on the platform.
//!
//! This is the native core: spawn an interpreter, feed it the
//! script over stdin, and return the (output-capped) result. A
//! future layer adds the RPC-from-script callback so the Macro can
//! invoke gated Relix tools mid-script and a turn budget that
//! refunds the collapsed steps — but the run-a-script-cheaply spine
//! is here.
//!
//! Like a Rig, a Macro is thin by governance: Relix can't see what
//! the script does internally, so it must run inside a Relix-managed
//! sandbox — the box is the boundary.

/// A Macro to run: an interpreter (+ args) fed a `script` over
/// stdin, with the output capped to `max_output_bytes`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MacroSpec {
    /// The interpreter binary (e.g. `python3`, `bash`, `sh`).
    pub interpreter: String,
    /// Interpreter arguments (often empty — the script comes on
    /// stdin).
    pub args: Vec<String>,
    /// The script, piped to the interpreter's stdin.
    pub script: String,
    /// Cap on returned `stdout` — the whole point is to keep only a
    /// small result in context, not the firehose.
    pub max_output_bytes: usize,
}

impl MacroSpec {
    pub fn new(interpreter: impl Into<String>, script: impl Into<String>) -> Self {
        Self {
            interpreter: interpreter.into(),
            args: Vec::new(),
            script: script.into(),
            max_output_bytes: 64 * 1024,
        }
    }

    pub fn with_args(mut self, args: Vec<String>) -> Self {
        self.args = args;
        self
    }

    pub fn with_max_output_bytes(mut self, n: usize) -> Self {
        self.max_output_bytes = n;
        self
    }
}

/// The result of a Macro run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MacroResult {
    /// Captured stdout, capped to `max_output_bytes` (raw — not
    /// trimmed — so the cap is exact).
    pub stdout: String,
    /// Captured stderr (trimmed, for display).
    pub stderr: String,
    /// The process exit code, or `None` if it never produced one
    /// (spawn/wait failure or killed by signal).
    pub exit_code: Option<i32>,
    /// Did the process exit cleanly (status 0)?
    pub success: bool,
    /// Was `stdout` truncated to fit `max_output_bytes`?
    pub truncated: bool,
}

impl MacroResult {
    fn failed(stderr: String) -> Self {
        Self {
            stdout: String::new(),
            stderr,
            exit_code: None,
            success: false,
            truncated: false,
        }
    }
}

/// Run a Macro: spawn the interpreter, feed it the script over
/// stdin, capture stdout/stderr, cap stdout, and report the result.
/// Never panics — a spawn / wait failure is a `success = false`
/// result with the reason on `stderr`.
pub fn run_macro(spec: &MacroSpec) -> MacroResult {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let mut child = match Command::new(&spec.interpreter)
        .args(&spec.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return MacroResult::failed(format!("spawn {}: {e}", spec.interpreter)),
    };

    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(spec.script.as_bytes());
        // stdin closes (EOF) when dropped at the end of this block.
    }

    let output = match child.wait_with_output() {
        Ok(o) => o,
        Err(e) => return MacroResult::failed(format!("wait {}: {e}", spec.interpreter)),
    };

    let mut stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let truncated = stdout.len() > spec.max_output_bytes;
    if truncated {
        let mut end = spec.max_output_bytes;
        while end > 0 && !stdout.is_char_boundary(end) {
            end -= 1;
        }
        stdout.truncate(end);
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

    MacroResult {
        stdout,
        stderr,
        exit_code: output.status.code(),
        success: output.status.success(),
        truncated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Cross-platform "run this command line" spec.
    fn cmd_spec(line: &str, cap: usize) -> MacroSpec {
        if cfg!(windows) {
            MacroSpec::new("cmd", "")
                .with_args(vec!["/C".into(), line.into()])
                .with_max_output_bytes(cap)
        } else {
            MacroSpec::new("sh", "")
                .with_args(vec!["-c".into(), line.into()])
                .with_max_output_bytes(cap)
        }
    }

    #[test]
    fn run_macro_executes_a_multi_step_script_and_returns_stdout() {
        // Two steps in one call — the whole point of a Macro.
        let line = if cfg!(windows) {
            "echo one& echo two"
        } else {
            "echo one; echo two"
        };
        let r = run_macro(&cmd_spec(line, 1024));
        assert!(r.success, "stderr: {}", r.stderr);
        assert!(
            r.stdout.contains("one") && r.stdout.contains("two"),
            "stdout: {:?}",
            r.stdout
        );
        assert!(!r.truncated);
    }

    #[test]
    fn run_macro_caps_output() {
        let r = run_macro(&cmd_spec("echo abcdefghij", 4));
        assert!(r.truncated);
        assert!(r.stdout.len() <= 4, "stdout: {:?}", r.stdout);
    }

    #[test]
    fn run_macro_reports_spawn_failure_without_panicking() {
        let spec = MacroSpec::new("nonexistent-interpreter-xyzzy", "print('hi')");
        let r = run_macro(&spec);
        assert!(!r.success);
        assert!(r.exit_code.is_none());
        assert!(r.stderr.contains("spawn"));
    }

    #[test]
    fn run_macro_pipes_the_script_over_stdin() {
        // On a POSIX shell, the script body itself comes via stdin.
        if cfg!(unix) {
            let spec = MacroSpec::new("sh", "echo from-stdin-script");
            let r = run_macro(&spec);
            assert!(r.success, "stderr: {}", r.stderr);
            assert!(r.stdout.contains("from-stdin-script"));
        }
    }
}
