//! Bounded scripted-child collection, independent of nextest's backstop (ADR-0037 §1).
use std::io;
use std::process::{Output, Stdio};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

use super::environment::Command;

/// Scripted peers admit connections for ten seconds; child runs are bounded too.
pub const CLI_LIMIT: Duration = Duration::from_secs(10);

async fn read(mut pipe: impl AsyncRead + Unpin) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    pipe.read_to_end(&mut bytes).await?;
    Ok(bytes)
}

/// Synchronous test entry point. One deadline covers spawn, input, exit and both
/// output pipes. Collect concurrently so full pipes cannot block child exit.
/// Timeout kills and reaps only this owned child, with a separate cleanup bound.
pub fn output(command: Command, input: Option<Vec<u8>>, limit: Duration) -> io::Result<Output> {
    let deadline = Instant::now() + limit;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let mut command = tokio::process::Command::from(command.into_std());
        command
            .kill_on_drop(true)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            });
        let mut child = command.spawn()?;
        let mut stdin = child.stdin.take();
        let stdout = child.stdout.take().expect("captured stdout");
        let stderr = child.stderr.take().expect("captured stderr");
        let collect = async {
            let (status, (), stdout, stderr) = tokio::try_join!(
                child.wait(),
                async {
                    if let Some(bytes) = input {
                        stdin
                            .as_mut()
                            .expect("piped input")
                            .write_all(&bytes)
                            .await?;
                    }
                    drop(stdin);
                    Ok::<_, io::Error>(())
                },
                read(stdout),
                read(stderr),
            )?;
            Ok(Output {
                status,
                stdout,
                stderr,
            })
        };
        match tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), collect).await {
            Ok(Ok(output)) => Ok(output),
            result => {
                let _ = child.start_kill();
                let reaped = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
                if !matches!(reaped, Ok(Ok(_))) {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "scripted child cleanup did not complete",
                    ));
                }
                match result {
                    Ok(Err(error)) => Err(error),
                    Err(_) => Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "scripted child exceeded its command deadline",
                    )),
                    Ok(Ok(_)) => unreachable!(),
                }
            }
        }
    })
}
