//! Safe process-environment helpers shared by terminal and generic tools.

use std::ffi::OsStr;
use std::io;
use std::process::{Child, Command};

pub fn command(program: impl AsRef<OsStr>) -> Command {
    Command::new(program)
}

pub fn spawn(command: &mut Command) -> io::Result<Child> {
    command.envs(shell_environment());
    command.spawn()
}

pub fn shell_environment() -> Vec<(String, String)> {
    std::env::vars().collect()
}

pub fn default_terminal_shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| {
        if cfg!(windows) {
            "cmd.exe".into()
        } else {
            "/bin/sh".into()
        }
    })
}

pub fn refresh_from_default_shell() {}
