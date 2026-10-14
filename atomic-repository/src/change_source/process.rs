use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use super::ChangeSourceError;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    pub current_dir: PathBuf,
    pub env: BTreeMap<String, String>,
    pub stdin: Vec<u8>,
    pub timeout: Duration,
    pub max_output_bytes: usize,
}

impl CommandSpec {
    pub fn new(program: impl Into<String>, current_dir: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            current_dir: current_dir.into(),
            env: BTreeMap::new(),
            stdin: Vec::new(),
            timeout: Duration::from_secs(5),
            max_output_bytes: 16 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandOutput {
    pub status: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

pub trait CommandRunner: Send + Sync {
    fn run(&self, spec: &CommandSpec) -> Result<CommandOutput, ChangeSourceError>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ProcessCommandRunner;

impl CommandRunner for ProcessCommandRunner {
    fn run(&self, spec: &CommandSpec) -> Result<CommandOutput, ChangeSourceError> {
        let mut command = Command::new(&spec.program);
        command
            .args(&spec.args)
            .current_dir(&spec.current_dir)
            .envs(&spec.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = command
            .spawn()
            .map_err(|error| ChangeSourceError::Unavailable(error.to_string()))?;

        let stdin = child.stdin.take();
        let input = spec.stdin.clone();
        let input_thread = thread::spawn(move || -> std::io::Result<()> {
            if let Some(mut stdin) = stdin {
                stdin.write_all(&input)?;
            }
            Ok(())
        });

        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let stdout_thread = thread::spawn(move || read_all(stdout));
        let stderr_thread = thread::spawn(move || read_all(stderr));

        let started = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if started.elapsed() < spec.timeout => {
                    thread::sleep(Duration::from_millis(10));
                }
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = input_thread.join();
                    let _ = stdout_thread.join();
                    let _ = stderr_thread.join();
                    return Err(ChangeSourceError::Timeout {
                        milliseconds: spec.timeout.as_millis() as u64,
                    });
                }
                Err(error) => {
                    return Err(ChangeSourceError::Unavailable(error.to_string()));
                }
            }
        };

        input_thread
            .join()
            .map_err(|_| ChangeSourceError::Unavailable("stdin writer panicked".into()))?
            .map_err(|error| ChangeSourceError::Unavailable(error.to_string()))?;
        let stdout = join_reader(stdout_thread)?;
        let stderr = join_reader(stderr_thread)?;
        if stdout.len().saturating_add(stderr.len()) > spec.max_output_bytes {
            return Err(ChangeSourceError::Overflow {
                limit: spec.max_output_bytes,
            });
        }

        Ok(CommandOutput {
            status: status.code().unwrap_or(-1),
            stdout,
            stderr,
        })
    }
}

fn read_all(mut reader: impl Read) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn join_reader(
    thread: thread::JoinHandle<std::io::Result<Vec<u8>>>,
) -> Result<Vec<u8>, ChangeSourceError> {
    thread
        .join()
        .map_err(|_| ChangeSourceError::Unavailable("process output reader panicked".into()))?
        .map_err(|error| ChangeSourceError::Unavailable(error.to_string()))
}
