use crate::protocol::Error;
use serde_json::Value;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

pub(crate) struct JsonlProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl JsonlProcess {
    pub(crate) async fn spawn_with_cwd(
        command: &str,
        args: &[&str],
        cwd: Option<&std::path::Path>,
    ) -> Result<Self, Error> {
        let mut command_builder = Command::new(command);
        command_builder.args(args);
        if let Some(cwd) = cwd {
            command_builder.current_dir(cwd);
        }
        let mut child = command_builder
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| Error::Backend(format!("failed to start {command}: {error}")))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::Backend("JSONL process stdin is unavailable".to_owned()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::Backend("JSONL process stdout is unavailable".to_owned()))?;

        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
        })
    }

    pub(crate) async fn write(&mut self, message: &Value) -> Result<(), Error> {
        let mut line = serde_json::to_vec(message)
            .map_err(|error| Error::Backend(format!("failed to encode JSONL message: {error}")))?;
        line.push(b'\n');
        self.stdin
            .write_all(&line)
            .await
            .map_err(|error| Error::Backend(format!("failed to write JSONL message: {error}")))?;
        self.stdin
            .flush()
            .await
            .map_err(|error| Error::Backend(format!("failed to flush JSONL message: {error}")))
    }

    pub(crate) async fn read(&mut self) -> Result<Option<Value>, Error> {
        let mut line = String::new();
        let bytes =
            self.stdout.read_line(&mut line).await.map_err(|error| {
                Error::Backend(format!("failed to read JSONL message: {error}"))
            })?;
        if bytes == 0 {
            return Ok(None);
        }

        serde_json::from_str(line.trim_end())
            .map(Some)
            .map_err(|error| Error::Backend(format!("invalid JSONL message: {error}")))
    }

    pub(crate) async fn close(&mut self) -> Result<(), Error> {
        if self
            .child
            .try_wait()
            .map_err(|error| Error::Backend(format!("failed to inspect JSONL process: {error}")))?
            .is_none()
        {
            self.child.kill().await.map_err(|error| {
                Error::Backend(format!("failed to stop JSONL process: {error}"))
            })?;
        }
        self.child
            .wait()
            .await
            .map_err(|error| Error::Backend(format!("failed to reap JSONL process: {error}")))?;
        Ok(())
    }
}
