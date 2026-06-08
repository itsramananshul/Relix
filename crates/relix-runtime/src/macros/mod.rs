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
    /// Working directory to run in (the Brief's **Bench**, normally).
    /// `None` inherits the parent's cwd.
    pub cwd: Option<String>,
    /// Extra environment variables — the *scoped* set the Cell hands
    /// the script (a bridge token, the Brief id, …). Applied on top
    /// of the inherited environment.
    pub env: Vec<(String, String)>,
}

impl MacroSpec {
    pub fn new(interpreter: impl Into<String>, script: impl Into<String>) -> Self {
        Self {
            interpreter: interpreter.into(),
            args: Vec::new(),
            script: script.into(),
            max_output_bytes: 64 * 1024,
            cwd: None,
            env: Vec::new(),
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

    /// Run the Macro in `dir` (the Brief's Bench).
    pub fn with_cwd(mut self, dir: impl Into<String>) -> Self {
        self.cwd = Some(dir.into());
        self
    }

    /// Add a scoped environment variable handed to the script.
    pub fn with_env(mut self, env: Vec<(String, String)>) -> Self {
        self.env = env;
        self
    }

    /// Is this Macro's interpreter on `allow`? Matched on the final
    /// path component — so an absolute path (`/usr/bin/python3`,
    /// `C:\Python\python3.exe`) still matches `python3` —
    /// case-insensitively, ignoring a trailing `.exe`. An empty
    /// allowlist denies everything (deny-by-default).
    pub fn interpreter_allowed(&self, allow: &[&str]) -> bool {
        let base = Self::base_name(&self.interpreter);
        allow.iter().any(|a| Self::base_name(a) == base)
    }

    fn base_name(s: &str) -> String {
        let last = s.rsplit(['/', '\\']).next().unwrap_or(s);
        let last = last
            .strip_suffix(".exe")
            .or_else(|| last.strip_suffix(".EXE"))
            .unwrap_or(last);
        last.to_ascii_lowercase()
    }
}

/// A Macro was refused *before* running — its interpreter isn't on
/// the allowlist. Carries the offending interpreter for the error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MacroDenied {
    pub interpreter: String,
}

impl std::fmt::Display for MacroDenied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "interpreter '{}' is not on the Macro allowlist",
            self.interpreter
        )
    }
}

impl std::error::Error for MacroDenied {}

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

    let mut command = Command::new(&spec.interpreter);
    command
        .args(&spec.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(dir) = &spec.cwd {
        command.current_dir(dir);
    }
    if !spec.env.is_empty() {
        command.envs(spec.env.iter().map(|(k, v)| (k, v)));
    }
    let mut child = match command.spawn() {
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

/// The line prefix a Macro uses to request a Relix tool call
/// mid-script (the RPC-to-tools sentinel). A script prints
/// `@relix-call method|args` to stdout; the runtime intercepts
/// those lines, dispatches each through the gated bridge (using the
/// Macro's bridge token), and the remaining stdout is the result.
pub const MACRO_CALL_SENTINEL: &str = "@relix-call ";

/// A tool call a Macro requested via [`MACRO_CALL_SENTINEL`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MacroToolCall {
    pub method: String,
    /// Everything after the first `|` — passed through verbatim as
    /// the call's pipe-delimited args (empty when none).
    pub args: String,
}

/// Split a Macro's stdout into (requested tool calls, residual
/// output). Lines beginning with [`MACRO_CALL_SENTINEL`] are parsed
/// as `method|args` and pulled out; everything else is preserved (in
/// order, minus a trailing newline) as the residual result. A
/// sentinel line with a blank method is ignored. This is the pure
/// parse half of Macro RPC — dispatch of the calls layers on top.
pub fn extract_tool_calls(stdout: &str) -> (Vec<MacroToolCall>, String) {
    // The sentinel keyword without the trailing space, so a bare
    // `@relix-call` line (empty call) is still recognised — and
    // dropped — rather than leaking into the residual output.
    let keyword = MACRO_CALL_SENTINEL.trim_end();
    let mut calls = Vec::new();
    let mut residual_lines = Vec::new();
    for line in stdout.lines() {
        let after = line
            .strip_prefix(keyword)
            // Only a sentinel if followed by whitespace or end-of-line
            // (so `@relix-callbroken` is ordinary output).
            .filter(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace));
        match after {
            Some(rest) => {
                let rest = rest.trim();
                let (method, args) = match rest.split_once('|') {
                    Some((m, a)) => (m.trim(), a),
                    None => (rest, ""),
                };
                if !method.is_empty() {
                    calls.push(MacroToolCall {
                        method: method.to_string(),
                        args: args.to_string(),
                    });
                }
                // A sentinel line (even an empty one) is never residual.
            }
            None => residual_lines.push(line),
        }
    }
    (calls, residual_lines.join("\n"))
}

/// Run a Macro only if its interpreter is on `allow` (see
/// [`MacroSpec::interpreter_allowed`]); otherwise refuse *before*
/// spawning anything. This is the execute_code safety gate — the
/// caller decides which interpreters a Cell may run, and an
/// off-list interpreter never reaches `Command::spawn`.
pub fn run_macro_guarded(spec: &MacroSpec, allow: &[&str]) -> Result<MacroResult, MacroDenied> {
    if !spec.interpreter_allowed(allow) {
        return Err(MacroDenied {
            interpreter: spec.interpreter.clone(),
        });
    }
    Ok(run_macro(spec))
}

/// Run a guarded Macro and split its captured stdout into the tool
/// calls it requested (via the [`MACRO_CALL_SENTINEL`]) and the
/// residual result. The returned [`MacroResult`] has its `stdout`
/// replaced by the residual (sentinel lines removed); the caller
/// dispatches the [`MacroToolCall`]s through the gated bridge. Same
/// allowlist refusal as [`run_macro_guarded`].
pub fn run_macro_rpc(
    spec: &MacroSpec,
    allow: &[&str],
) -> Result<(Vec<MacroToolCall>, MacroResult), MacroDenied> {
    let result = run_macro_guarded(spec, allow)?;
    let (calls, residual) = extract_tool_calls(&result.stdout);
    Ok((
        calls,
        MacroResult {
            stdout: residual,
            ..result
        },
    ))
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
    fn run_macro_rpc_splits_calls_from_residual() {
        // Emit one sentinel call line and one plain output line.
        let line = if cfg!(windows) {
            "echo @relix-call rig.list& echo hello"
        } else {
            "printf '@relix-call rig.list\\nhello\\n'"
        };
        let allow = if cfg!(windows) { "cmd" } else { "sh" };
        let (calls, result) = run_macro_rpc(&cmd_spec(line, 4096), &[allow]).expect("allowed");
        assert!(result.success, "stderr: {}", result.stderr);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].method, "rig.list");
        // The sentinel line was removed from the residual stdout.
        assert!(result.stdout.contains("hello"));
        assert!(!result.stdout.contains("@relix-call"));

        // Off-list interpreter still refused.
        assert!(run_macro_rpc(&cmd_spec("echo x", 64), &["python3"]).is_err());
    }

    #[test]
    fn extract_tool_calls_pulls_sentinel_lines_from_residual() {
        let stdout = "\
line one
@relix-call brief.comment|b1|agt|hello
middle output
@relix-call brief.move|b1|in_progress
@relix-call
last line";
        let (calls, residual) = extract_tool_calls(stdout);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].method, "brief.comment");
        assert_eq!(calls[0].args, "b1|agt|hello");
        assert_eq!(calls[1].method, "brief.move");
        assert_eq!(calls[1].args, "b1|in_progress");
        // Residual keeps the non-sentinel lines in order.
        assert_eq!(residual, "line one\nmiddle output\nlast line");

        // A call with no args parses to an empty args string.
        let (c2, _) = extract_tool_calls("@relix-call rig.list");
        assert_eq!(c2.len(), 1);
        assert_eq!(c2[0].method, "rig.list");
        assert_eq!(c2[0].args, "");

        // No sentinels → no calls, residual == input (sans trailing \n).
        let (c3, r3) = extract_tool_calls("just\noutput\n");
        assert!(c3.is_empty());
        assert_eq!(r3, "just\noutput");
    }

    #[test]
    fn interpreter_allowlist_matches_basename_path_and_exe() {
        let allow = ["python3", "bash"];
        assert!(MacroSpec::new("python3", "").interpreter_allowed(&allow));
        // Absolute path still matches on the basename.
        assert!(MacroSpec::new("/usr/bin/python3", "").interpreter_allowed(&allow));
        assert!(MacroSpec::new("C:\\tools\\bash.exe", "").interpreter_allowed(&allow));
        // Case-insensitive.
        assert!(MacroSpec::new("BASH", "").interpreter_allowed(&allow));
        // Not on the list.
        assert!(!MacroSpec::new("ruby", "").interpreter_allowed(&allow));
        // Empty allowlist denies everything.
        assert!(!MacroSpec::new("python3", "").interpreter_allowed(&[]));
    }

    #[test]
    fn guarded_refuses_offlist_interpreter_without_spawning() {
        // A bogus binary that WOULD fail to spawn — but the guard
        // must reject it first, so we get MacroDenied, not a spawn
        // error.
        let spec = MacroSpec::new("nonexistent-interpreter-xyzzy", "whatever");
        let err = run_macro_guarded(&spec, &["python3"]).unwrap_err();
        assert_eq!(err.interpreter, "nonexistent-interpreter-xyzzy");
        assert!(err.to_string().contains("allowlist"));
    }

    #[test]
    fn guarded_runs_an_allowlisted_interpreter() {
        let spec = cmd_spec("echo ok", 64);
        let allow = if cfg!(windows) { "cmd" } else { "sh" };
        let r = run_macro_guarded(&spec, &[allow]).expect("allowed");
        assert!(r.success, "stderr: {}", r.stderr);
        assert!(r.stdout.contains("ok"));
    }

    #[test]
    fn run_macro_applies_scoped_env() {
        let line = if cfg!(windows) {
            "echo %RELIX_MTEST%"
        } else {
            "echo $RELIX_MTEST"
        };
        let spec = cmd_spec(line, 64).with_env(vec![("RELIX_MTEST".into(), "hello42".into())]);
        let r = run_macro(&spec);
        assert!(r.success, "stderr: {}", r.stderr);
        assert!(r.stdout.contains("hello42"), "stdout: {:?}", r.stdout);
    }

    #[test]
    fn run_macro_runs_in_the_given_cwd() {
        // Use the OS temp dir as a known, existing working directory.
        let tmp = std::env::temp_dir();
        let line = if cfg!(windows) { "cd" } else { "pwd" };
        let spec = cmd_spec(line, 4096).with_cwd(tmp.to_string_lossy().to_string());
        let r = run_macro(&spec);
        assert!(r.success, "stderr: {}", r.stderr);
        // The printed path should reference the temp dir's final
        // component (robust against symlinked /tmp vs /private/tmp).
        let leaf = tmp
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        if !leaf.is_empty() {
            assert!(
                r.stdout.to_lowercase().contains(&leaf.to_lowercase()),
                "cwd not honoured: {:?} (leaf {leaf})",
                r.stdout
            );
        }
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
