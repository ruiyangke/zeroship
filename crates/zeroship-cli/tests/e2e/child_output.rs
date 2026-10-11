//! A spawned process's output, read on threads as it is written, so a suite
//! waits on the lines it needs and on the process's exit rather than polling.

use std::io::{BufRead, BufReader, Read};
use std::process::Child;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::time::{Duration, Instant};

/// Every line a child writes to its stdout and stderr. The output closes once
/// both pipes have closed, which is when the child, and every process that
/// inherited the pipes from it, has exited.
pub struct ChildOutput {
    lines: Receiver<String>,
    seen: Vec<String>,
    closed: bool,
}

impl ChildOutput {
    /// Read `child`'s stdout and stderr, which it must have been spawned with
    /// piped.
    pub fn capture(child: &mut Child) -> Self {
        let (sender, lines) = mpsc::channel();
        forward(child.stdout.take().expect("stdout is piped"), sender.clone());
        forward(child.stderr.take().expect("stderr is piped"), sender);
        Self {
            lines,
            seen: Vec::new(),
            closed: false,
        }
    }

    /// Whether a line containing `needle` arrives within `timeout`. Lines that
    /// already arrived count.
    pub fn wait_for(&mut self, needle: &str, timeout: Duration) -> bool {
        if self.seen.iter().any(|line| line.contains(needle)) {
            return true;
        }
        let deadline = Instant::now() + timeout;
        while let Some(line) = self.next_before(deadline) {
            if line.contains(needle) {
                return true;
            }
        }
        false
    }

    /// Whether the output closes within `timeout`, reading every line that
    /// arrives meanwhile.
    pub fn closes_within(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while self.next_before(deadline).is_some() {}
        self.closed
    }

    /// Whether the output is still open after waiting at most `step` for a
    /// line, for a caller that acts between waits.
    pub fn open_after(&mut self, step: Duration) -> bool {
        let _ = self.next_before(Instant::now() + step);
        !self.closed
    }

    /// Everything the child wrote so far.
    pub fn text(&mut self) -> String {
        loop {
            match self.lines.try_recv() {
                Ok(line) => self.seen.push(line),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.closed = true;
                    break;
                }
            }
        }
        self.seen.join("\n")
    }

    fn next_before(&mut self, deadline: Instant) -> Option<String> {
        if self.closed {
            return None;
        }
        let left = deadline.checked_duration_since(Instant::now())?;
        match self.lines.recv_timeout(left) {
            Ok(line) => {
                self.seen.push(line.clone());
                Some(line)
            }
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => {
                self.closed = true;
                None
            }
        }
    }
}

/// Send each line read from `pipe` until it closes.
fn forward(pipe: impl Read + Send + 'static, sender: mpsc::Sender<String>) {
    std::thread::spawn(move || {
        for line in BufReader::new(pipe).lines() {
            let Ok(line) = line else { return };
            if sender.send(line).is_err() {
                return;
            }
        }
    });
}
