//! `shell` tool: subprocess execution with output capture, timeout, and
//! cancellation. Shell syntax mirrors the host platform.

use super::{ExecOutcome, ExecutionContext, ExecutionMode, Tool, ToolError, ToolSpec};
use crate::model::{ToolCall, ToolResultPart};
use serde_json::{Value, json};
use std::io::{self, Read};
use std::process::Stdio;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

pub struct ShellTool {
    timeout: Duration,
    max_output: usize,
}

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);
const DEFAULT_MAX_OUTPUT: usize = 200 * 1024;
const DRAIN_AFTER_EXIT: Duration = Duration::from_millis(100);

impl ShellTool {
    pub fn unbound() -> Self {
        ShellTool {
            timeout: DEFAULT_TIMEOUT,
            max_output: DEFAULT_MAX_OUTPUT,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

impl Default for ShellTool {
    fn default() -> Self {
        Self::unbound()
    }
}

impl Tool for ShellTool {
    fn name(&self) -> &'static str {
        "shell"
    }

    fn spec(&self) -> &ToolSpec {
        static SPEC: LazyLock<ToolSpec> = LazyLock::new(|| ToolSpec {
            name: "shell".into(),
            description: "Run a shell command in the workspace. Returns combined stdout/stderr."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" },
                    "timeout_secs": { "type": "integer", "description": "Optional per-call timeout" }
                },
                "required": ["command"]
            }),
            required: vec!["command".into()],
        });
        &SPEC
    }

    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Sequential
    }

    fn execute<'a>(
        &'a self,
        call: &'a ToolCall,
        exec: ExecutionContext<'a>,
    ) -> futures::future::BoxFuture<'a, Result<ExecOutcome, ToolError>> {
        Box::pin(async move {
            if !exec.ctx.allow_shell {
                return Err(ToolError::Failed("shell execution is disabled".into()));
            }
            let command = call
                .arguments
                .get("command")
                .and_then(Value::as_str)
                .ok_or_else(|| ToolError::InvalidArguments("command must be a string".into()))?;
            let timeout = call
                .arguments
                .get("timeout_secs")
                .and_then(Value::as_u64)
                .map(Duration::from_secs)
                .unwrap_or(self.timeout);
            exec.check_cancelled()?;

            #[cfg(unix)]
            let mut proc = {
                use std::os::unix::process::CommandExt as _;
                let mut c = std::process::Command::new("/bin/sh");
                c.arg("-c").arg(command).current_dir(&exec.ctx.cwd);
                c.process_group(0);
                c.stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                c
            };
            #[cfg(not(unix))]
            let mut proc = {
                let mut c = std::process::Command::new("cmd");
                c.args(["/C", command])
                    .current_dir(&exec.ctx.cwd)
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                c
            };

            let mut child = proc
                .spawn()
                .map_err(|e| ToolError::Failed(format!("spawn: {e}")))?;
            let mut pipes = PipeDrain::take(&mut child, self.max_output)?;
            let deadline = Instant::now() + timeout;
            loop {
                pipes.pump(Duration::from_millis(10))?;
                if exec.cancel.is_cancelled() {
                    terminate_child(&mut child);
                    let _ = child.wait();
                    pipes.finish(DRAIN_AFTER_EXIT);
                    return Err(ToolError::Cancelled);
                }
                match child.try_wait() {
                    Ok(Some(status)) => {
                        pipes.finish(DRAIN_AFTER_EXIT);
                        let text = truncate_chars(&pipes.combined(), self.max_output);
                        return Ok(ExecOutcome {
                            parts: vec![ToolResultPart::Text(format!(
                                "exit {}\n{}",
                                status.code().unwrap_or(-1),
                                text
                            ))],
                            details: Some(json!({ "exit": status.code() })),
                            terminate: false,
                        });
                    }
                    Ok(None) => {
                        if Instant::now() >= deadline {
                            terminate_child(&mut child);
                            let _ = child.wait();
                            pipes.finish(DRAIN_AFTER_EXIT);
                            return Err(ToolError::Failed(format!(
                                "command exceeded {}s timeout",
                                timeout.as_secs()
                            )));
                        }
                        futures_timer::Delay::new(Duration::from_millis(10)).await;
                    }
                    Err(e) => {
                        terminate_child(&mut child);
                        let _ = child.wait();
                        pipes.finish(DRAIN_AFTER_EXIT);
                        return Err(ToolError::Failed(format!("wait: {e}")));
                    }
                }
            }
        })
    }
}

struct PipeDrain {
    stdout: Option<std::fs::File>,
    stderr: Option<std::fs::File>,
    out: Vec<u8>,
    err: Vec<u8>,
    max: usize,
}

impl PipeDrain {
    fn take(child: &mut std::process::Child, max: usize) -> Result<Self, ToolError> {
        let stdout = child
            .stdout
            .take()
            .map(to_file)
            .transpose()
            .map_err(|e| ToolError::Failed(format!("stdout: {e}")))?;
        let stderr = child
            .stderr
            .take()
            .map(to_file)
            .transpose()
            .map_err(|e| ToolError::Failed(format!("stderr: {e}")))?;
        for pipe in [&stdout, &stderr].into_iter().flatten() {
            set_nonblocking(pipe).map_err(|e| ToolError::Failed(format!("pipe: {e}")))?;
        }
        Ok(Self {
            stdout,
            stderr,
            out: Vec::new(),
            err: Vec::new(),
            max,
        })
    }

    fn pump(&mut self, timeout: Duration) -> Result<(), ToolError> {
        poll_pipes(
            [&mut self.stdout, &mut self.stderr],
            [&mut self.out, &mut self.err],
            self.max,
            timeout,
        )
        .map_err(|e| ToolError::Failed(format!("poll: {e}")))
    }

    fn finish(&mut self, budget: Duration) {
        let deadline = Instant::now() + budget;
        while Instant::now() < deadline && (self.stdout.is_some() || self.stderr.is_some()) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if self.pump(remaining.min(Duration::from_millis(20))).is_err() {
                break;
            }
        }
        self.stdout = None;
        self.stderr = None;
    }

    fn combined(&self) -> String {
        combine_output(
            String::from_utf8_lossy(&self.out).into_owned(),
            String::from_utf8_lossy(&self.err).into_owned(),
        )
    }
}

fn to_file<T: IntoRawOwnedFd>(pipe: T) -> io::Result<std::fs::File> {
    pipe.into_file()
}

trait IntoRawOwnedFd {
    fn into_file(self) -> io::Result<std::fs::File>;
}

#[cfg(unix)]
impl IntoRawOwnedFd for std::process::ChildStdout {
    fn into_file(self) -> io::Result<std::fs::File> {
        use std::os::unix::io::{FromRawFd, IntoRawFd};
        let fd = self.into_raw_fd();
        // SAFETY: exclusive ownership of the child's stdout fd.
        Ok(unsafe { std::fs::File::from_raw_fd(fd) })
    }
}

#[cfg(unix)]
impl IntoRawOwnedFd for std::process::ChildStderr {
    fn into_file(self) -> io::Result<std::fs::File> {
        use std::os::unix::io::{FromRawFd, IntoRawFd};
        let fd = self.into_raw_fd();
        // SAFETY: exclusive ownership of the child's stderr fd.
        Ok(unsafe { std::fs::File::from_raw_fd(fd) })
    }
}

#[cfg(not(unix))]
impl IntoRawOwnedFd for std::process::ChildStdout {
    fn into_file(self) -> io::Result<std::fs::File> {
        Err(io::Error::other("non-unix shell pipes are not supported"))
    }
}

#[cfg(not(unix))]
impl IntoRawOwnedFd for std::process::ChildStderr {
    fn into_file(self) -> io::Result<std::fs::File> {
        Err(io::Error::other("non-unix shell pipes are not supported"))
    }
}

#[cfg(unix)]
fn set_nonblocking(file: &std::fs::File) -> io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let fd = file.as_raw_fd();
    // SAFETY: `fd` is a live pipe we own.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        if libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn set_nonblocking(_file: &std::fs::File) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn poll_pipes(
    pipes: [&mut Option<std::fs::File>; 2],
    bufs: [&mut Vec<u8>; 2],
    max: usize,
    timeout: Duration,
) -> io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let mut fds = [libc::pollfd {
        fd: -1,
        events: libc::POLLIN,
        revents: 0,
    }; 2];
    for (i, pipe) in pipes.iter().enumerate() {
        if let Some(file) = pipe.as_ref() {
            fds[i].fd = file.as_raw_fd();
        }
    }
    if fds.iter().all(|fd| fd.fd < 0) {
        return Ok(());
    }
    let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
    // SAFETY: fds refer to open pipes we own for the duration of poll.
    let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, ms) };
    if n < 0 {
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::Interrupted {
            return Ok(());
        }
        return Err(err);
    }
    for i in 0..2 {
        if fds[i].fd < 0 {
            continue;
        }
        if fds[i].revents == 0 {
            continue;
        }
        if !read_available(pipes[i], bufs[i], max)? {
            *pipes[i] = None;
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn poll_pipes(
    pipes: [&mut Option<std::fs::File>; 2],
    bufs: [&mut Vec<u8>; 2],
    max: usize,
    _timeout: Duration,
) -> io::Result<()> {
    for i in 0..2 {
        if pipes[i].is_some() && !read_available(pipes[i], bufs[i], max)? {
            *pipes[i] = None;
        }
    }
    Ok(())
}

fn read_available(
    pipe: &mut Option<std::fs::File>,
    dest: &mut Vec<u8>,
    max: usize,
) -> io::Result<bool> {
    let Some(file) = pipe.as_mut() else {
        return Ok(false);
    };
    let mut buf = [0u8; 8192];
    loop {
        match file.read(&mut buf) {
            Ok(0) => return Ok(false),
            Ok(n) => {
                let keep = max.saturating_add(1).saturating_sub(dest.len()).min(n);
                dest.extend_from_slice(&buf[..keep]);
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => return Ok(true),
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }
}

fn combine_output(stdout: String, stderr: String) -> String {
    if stdout.is_empty() {
        stderr
    } else if stderr.is_empty() {
        stdout
    } else {
        format!("{stdout}\n{stderr}")
    }
}

fn terminate_child(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let pid = child.id() as libc::pid_t;
        // SAFETY: `pid` is the process-group leader created by `process_group(0)`.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    let _ = child.kill();
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n…(truncated)", &s[..end])
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::cancel::CancelToken;
    use crate::model::ToolCall;
    use crate::tools::{ExecutionContext, Tool, ToolContext};
    use std::time::Instant;

    fn exec<'a>(ctx: &'a ToolContext, cancel: &'a CancelToken) -> ExecutionContext<'a> {
        ExecutionContext {
            ctx,
            cancel: cancel.clone(),
        }
    }

    fn grandchild_command() -> &'static str {
        "(setsid sleep 30 >/dev/stdout 2>/dev/stderr &) && echo started && sleep 30"
    }

    fn call(command: &str) -> ToolCall {
        ToolCall {
            id: "s".into(),
            name: "shell".into(),
            arguments: json!({ "command": command }),
            thought_signature: None,
        }
    }

    #[test]
    fn escaped_grandchild_holding_pipes_does_not_block_timeout() {
        let tool = ShellTool::unbound().with_timeout(Duration::from_millis(250));
        let ctx = ToolContext::new(std::env::temp_dir());
        let cancel = CancelToken::new();
        let started = Instant::now();
        let result = futures::executor::block_on(
            tool.execute(&call(grandchild_command()), exec(&ctx, &cancel)),
        );
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "timeout returned in {elapsed:?}"
        );
        assert!(
            matches!(result, Err(ToolError::Failed(message)) if message.contains("timeout")),
            "unexpected shell result"
        );
    }

    #[test]
    fn escaped_grandchild_holding_pipes_does_not_block_cancel() {
        let tool = ShellTool::unbound().with_timeout(Duration::from_secs(30));
        let ctx = ToolContext::new(std::env::temp_dir());
        let cancel = CancelToken::new();
        let started = Instant::now();
        let call = call(grandchild_command());
        let ctx_exec = exec(&ctx, &cancel);
        let result = futures::executor::block_on(async {
            let running = tool.execute(&call, ctx_exec);
            let stop = async {
                futures_timer::Delay::new(Duration::from_millis(80)).await;
                cancel.cancel();
            };
            let (result, _) = futures::future::join(running, stop).await;
            result
        });
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "cancel returned in {elapsed:?}"
        );
        assert!(matches!(result, Err(ToolError::Cancelled)));
    }

    #[test]
    fn escaped_grandchild_does_not_block_normal_completion() {
        let tool = ShellTool::unbound().with_timeout(Duration::from_secs(2));
        let ctx = ToolContext::new(std::env::temp_dir());
        let cancel = CancelToken::new();
        let started = Instant::now();
        let result = futures::executor::block_on(tool.execute(
            &call("(setsid sleep 30 >/dev/stdout 2>/dev/stderr &) && echo started"),
            exec(&ctx, &cancel),
        ));
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "completion returned in {elapsed:?}"
        );
        let outcome = result.expect("command should complete");
        assert!(
            matches!(&outcome.parts[0], ToolResultPart::Text(text) if text.contains("started")),
            "missing started output"
        );
    }
}
