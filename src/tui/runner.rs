//! Background child process whose output lines stream into the TUI. The TUI
//! drives the `subx` CLI itself (`current_exe`), so every engine and tool path
//! stays in one place and the log pane shows exactly what a terminal would.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;

/// A running (or finished) child with its collected output.
pub struct Job {
    child: Child,
    rx: Receiver<String>,
    pub command: String,
    pub lines: Vec<String>,
    pub exit: Option<i32>,
}

fn pump(reader: impl std::io::Read + Send + 'static, tx: mpsc::Sender<String>) {
    thread::spawn(move || {
        for line in BufReader::new(reader).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
}

impl Job {
    /// Start `program args...` with stdout and stderr captured line by line.
    pub fn spawn(program: &Path, args: &[String]) -> std::io::Result<Self> {
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let (tx, rx) = mpsc::channel();
        if let Some(out) = child.stdout.take() {
            pump(out, tx.clone());
        }
        if let Some(err) = child.stderr.take() {
            pump(err, tx);
        }
        let command = std::iter::once(program.display().to_string())
            .chain(args.iter().map(|a| {
                if a.contains(' ') {
                    format!("\"{a}\"")
                } else {
                    a.clone()
                }
            }))
            .collect::<Vec<_>>()
            .join(" ");
        Ok(Self {
            child,
            rx,
            command,
            lines: Vec::new(),
            exit: None,
        })
    }

    /// Pull whatever arrived since the last call; returns how many lines were added.
    pub fn poll(&mut self) -> usize {
        let mut added = 0;
        // Empty and Disconnected both mean "nothing more right now".
        while let Ok(line) = self.rx.try_recv() {
            self.lines.push(line);
            added += 1;
        }
        if self.exit.is_none()
            && let Ok(Some(status)) = self.child.try_wait()
        {
            self.exit = Some(status.code().unwrap_or(-1));
        }
        added
    }

    pub fn running(&self) -> bool {
        self.exit.is_none()
    }

    /// Stop the child; its output so far is kept.
    pub fn kill(&mut self) {
        if self.running() {
            let _ = self.child.kill();
            let _ = self.child.wait();
            self.exit = Some(-1);
            self.lines.push("[cancelled]".to_string());
        }
    }

    /// Block until the child exits, draining its output (used by tests).
    #[cfg(test)]
    pub fn wait(&mut self) -> i32 {
        let status = self
            .child
            .wait()
            .map(|s| s.code().unwrap_or(-1))
            .unwrap_or(-1);
        // The pump threads drop their senders when the pipes close.
        while let Ok(line) = self.rx.recv() {
            self.lines.push(line);
        }
        self.exit = Some(status);
        status
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        if self.running() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streams_lines_and_exit_code() {
        // The test binary itself is a convenient child: `--list` prints test names.
        let exe = std::env::current_exe().unwrap();
        let mut job = Job::spawn(&exe, &["--list".to_string()]).unwrap();
        assert!(job.command.ends_with("--list"), "{}", job.command);
        let code = job.wait();
        assert_eq!(code, 0);
        assert!(
            job.lines
                .iter()
                .any(|l| l.contains("streams_lines_and_exit_code")),
            "{:?}",
            job.lines
        );
        assert!(!job.running());
        job.poll();
        job.kill(); // no-op once finished
        assert!(!job.lines.contains(&"[cancelled]".to_string()));
    }

    #[test]
    fn spawn_failure_is_an_error() {
        assert!(Job::spawn(Path::new("definitely-not-a-program-xyz"), &[]).is_err());
    }
}
