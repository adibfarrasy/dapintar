/// Spawns a JVM process with JDWP agent flags, parses the port from stdout,
/// and returns a connected `JdwpClient` ready for use.
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc::UnboundedReceiver;
use tracing::debug;

use crate::jdwp::{JdwpClient, JdwpEvent};

/// Holds the running JVM process. Dropping this struct kills the process.
pub struct JvmProcess {
    child: Option<Child>,
}

impl JvmProcess {
    /// Move the child process out of this wrapper.
    /// After calling this the Drop impl becomes a no-op, so the caller is
    /// responsible for managing the child's lifetime.
    pub fn take_child(&mut self) -> Child {
        self.child.take().expect("JvmProcess child already taken")
    }
}

impl Drop for JvmProcess {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            // Best-effort kill; ignore errors (process may have already exited).
            let _ = child.start_kill();
        }
    }
}

/// Spawns a JVM process, connects JDWP, and registers a `ClassPrepare` listener.
///
/// The JVM starts with `suspend=y`, so it pauses before executing `main_class`.
/// The caller must call `vm_resume()` (typically from `configurationDone`) to
/// let execution proceed.
///
/// Returns the JDWP client, the process handle, and the event receiver.
pub async fn launch(
    working_dir: &Path,
    classpath: &[PathBuf],
    main_class: &str,
) -> Result<(Arc<JdwpClient>, JvmProcess, UnboundedReceiver<JdwpEvent>)> {
    let cp_sep = if cfg!(windows) { ";" } else { ":" };
    let classpath_str = classpath
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(cp_sep);

    debug!("launching JVM: class={main_class} cp={classpath_str}");

    let mut child = Command::new("java")
        .current_dir(working_dir)
        .args([
            "-agentlib:jdwp=transport=dt_socket,server=y,suspend=y,address=0",
            "-cp",
            &classpath_str,
            main_class,
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("failed to spawn JVM process — is `java` on PATH?")?;

    // Read JVM stdout until we see the JDWP port advertisement.
    let stdout = child.stdout.take().expect("stdout is piped");
    let port = tokio::time::timeout(
        Duration::from_secs(30),
        parse_jdwp_port(BufReader::new(stdout)),
    )
    .await
    .context("timed out waiting for JVM to advertise JDWP port")??;

    debug!("JVM JDWP listening on port {port}");

    let addr = format!("127.0.0.1:{port}");
    let (client, event_rx) = JdwpClient::connect(&addr)
        .await
        .with_context(|| format!("failed to connect JDWP to {addr}"))?;

    let client = Arc::new(client);

    // Register ClassPrepare so deferred breakpoints can be installed when
    // a class is loaded later.
    client.event_request_set_class_prepare().await?;

    Ok((client, JvmProcess { child: Some(child) }, event_rx))
}

/// Reads lines from `reader` until it finds the JDWP port advertisement:
/// `Listening for transport dt_socket at address: <host:port | port>`
async fn parse_jdwp_port<R: tokio::io::AsyncRead + Unpin>(reader: BufReader<R>) -> Result<u16> {
    let mut lines = reader.lines();
    while let Some(line) = lines.next_line().await? {
        if let Some(addr) = line.strip_prefix("Listening for transport dt_socket at address: ") {
            let addr = addr.trim();
            // addr may be just "54321" or "127.0.0.1:54321"
            let port_str = match addr.rfind(':') {
                Some(i) => &addr[i + 1..],
                None => addr,
            };
            let port = port_str
                .parse::<u16>()
                .with_context(|| format!("could not parse JDWP port from '{addr}'"))?;
            return Ok(port);
        }
    }
    Err(anyhow!(
        "JVM stdout closed before JDWP port was advertised (bad main class or JVM crash?)"
    ))
}
