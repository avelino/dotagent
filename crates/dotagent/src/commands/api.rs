//! `dotagent api` — raw JSONL bridge for the daemon's local API.
//!
//! The bridge deliberately does not parse, render, or persist messages. It is
//! only a transport for scripts and TUIs that already speak the local API wire
//! format.

use std::{io, path::PathBuf};

use anyhow::{Context, Result};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// Keep the CLI bridge aligned with the local API's maximum line size. A
/// frame is rejected before any of its bytes reach the destination writer.
const MAX_FRAME_BYTES: usize = 64 * 1024;

/// Resolve the local API socket, honoring an explicit path before the shared
/// dotagent home layout.
pub(crate) fn resolve_socket_path(socket: Option<PathBuf>) -> PathBuf {
    socket.unwrap_or_else(|| dotagent_state::paths::home().join("api.sock"))
}

/// Bridge raw JSONL between stdin/stdout and the daemon's Unix socket.
pub async fn run(socket: Option<PathBuf>) -> Result<()> {
    let socket_path = resolve_socket_path(socket);
    let stream = UnixStream::connect(&socket_path)
        .await
        .with_context(|| format!("connecting to local API socket {}", socket_path.display()))?;

    bridge(stream, tokio::io::stdin(), tokio::io::stdout()).await
}

/// Keep both directions live: a server response must be readable before stdin
/// reaches EOF, while stdin EOF must half-close only the socket write side.
async fn bridge<R, W>(stream: UnixStream, stdin: R, stdout: W) -> Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin,
{
    let (socket_reader, socket_writer) = stream.into_split();
    let mut stdin_task = tokio::spawn(forward_stdin(stdin, socket_writer));
    let socket_task = forward_frames(socket_reader, stdout);
    tokio::pin!(socket_task);

    tokio::select! {
        stdin_result = &mut stdin_task => {
            let stdin_result = stdin_result.context("stdin forwarding task failed")?;
            stdin_result.context("forwarding stdin to local API socket")?;
            socket_task
                .await
                .context("forwarding local API socket to stdout")
        }
        socket_result = &mut socket_task => {
            // The server closed its side. Do not wait for stdin EOF before
            // returning; the socket is the lifetime boundary for this bridge.
            stdin_task.abort();
            socket_result.context("forwarding local API socket to stdout")
        }
    }
}

/// Forward complete frames without interpreting their bytes.
async fn forward_frames<R, W>(reader: R, mut writer: W) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut reader = BufReader::new(reader);

    loop {
        let Some(frame) = read_bounded_frame(&mut reader).await? else {
            return Ok(());
        };
        writer.write_all(&frame).await?;
        writer.flush().await?;
    }
}

/// Read one JSONL frame without buffering an unterminated peer forever.
async fn read_bounded_frame<R>(reader: &mut R) -> io::Result<Option<Vec<u8>>>
where
    R: AsyncBufRead + Unpin,
{
    let mut frame = Vec::with_capacity(MAX_FRAME_BYTES.min(4096));

    loop {
        let buffer = reader.fill_buf().await?;
        if buffer.is_empty() {
            return if frame.is_empty() {
                Ok(None)
            } else {
                Ok(Some(frame))
            };
        }

        let newline = buffer.iter().position(|byte| *byte == b'\n');
        let content_len = newline.unwrap_or(buffer.len());
        let remaining = MAX_FRAME_BYTES.saturating_sub(frame.len());
        if content_len > remaining {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("JSONL frame exceeds {MAX_FRAME_BYTES} bytes"),
            ));
        }

        frame.extend_from_slice(&buffer[..content_len]);
        let consumed = newline.map_or(buffer.len(), |index| index + 1);
        if newline.is_some() {
            frame.push(b'\n');
        }
        reader.consume(consumed);
        if newline.is_some() {
            return Ok(Some(frame));
        }
    }
}

/// Forward stdin and half-close the socket write side after its EOF.
async fn forward_stdin<R>(
    reader: R,
    mut writer: tokio::net::unix::OwnedWriteHalf,
) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
{
    forward_frames(reader, &mut writer).await?;
    writer.shutdown().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;
    use tempfile::tempdir;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixListener;
    use tokio::sync::Notify;

    #[test]
    fn default_socket_uses_dotagent_home() {
        assert_eq!(
            resolve_socket_path(None),
            dotagent_state::paths::home().join("api.sock")
        );
    }

    #[test]
    fn explicit_socket_path_wins() {
        let path = PathBuf::from("/tmp/dotagent-test-api.sock");
        assert_eq!(resolve_socket_path(Some(path.clone())), path);
    }

    #[tokio::test]
    async fn bridge_preserves_jsonl_bytes_and_half_closes_stdin() {
        let dir = tempdir().unwrap();
        let socket_path = dir.path().join("api.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read_half, mut write_half) = stream.into_split();
            let mut reader = BufReader::new(read_half);

            let mut first = Vec::new();
            let mut second = Vec::new();
            reader.read_until(b'\n', &mut first).await.unwrap();
            reader.read_until(b'\n', &mut second).await.unwrap();
            assert_eq!(first, b"{\"id\":1}\r\n");
            assert_eq!(second, b"{\"id\":2}\n");

            let mut trailing = Vec::new();
            reader.read_to_end(&mut trailing).await.unwrap();
            assert!(trailing.is_empty(), "unexpected bytes after JSONL input");

            write_half
                .write_all(b"{\"id\":\"1\",\"result\":{}}\n{\"event\":\"reply\"}\r\n")
                .await
                .unwrap();
            write_half.shutdown().await.unwrap();
        });

        let client = UnixStream::connect(&socket_path).await.unwrap();
        let (mut input_writer, input_reader) = tokio::io::duplex(4096);
        let input = tokio::spawn(async move {
            input_writer
                .write_all(b"{\"id\":1}\r\n{\"id\":2}\n")
                .await
                .unwrap();
            input_writer.shutdown().await.unwrap();
        });
        let (mut output_reader, output_writer) = tokio::io::duplex(4096);

        bridge(client, input_reader, output_writer).await.unwrap();

        let mut output = Vec::new();
        output_reader.read_to_end(&mut output).await.unwrap();
        assert_eq!(
            output,
            b"{\"id\":\"1\",\"result\":{}}\n{\"event\":\"reply\"}\r\n"
        );

        input.await.unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn bridge_forwards_socket_frames_before_stdin_eof() {
        let dir = tempdir().unwrap();
        let socket_path = dir.path().join("api.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read_half, mut write_half) = stream.into_split();
            let mut reader = BufReader::new(read_half);
            let mut request = Vec::new();
            reader.read_until(b'\n', &mut request).await.unwrap();
            assert_eq!(request, b"{\"id\":1}\n");

            write_half
                .write_all(b"{\"id\":1,\"result\":{}}\n")
                .await
                .unwrap();

            let mut trailing = Vec::new();
            reader.read_to_end(&mut trailing).await.unwrap();
            assert!(trailing.is_empty());
            write_half.shutdown().await.unwrap();
        });

        let client = UnixStream::connect(&socket_path).await.unwrap();
        let (mut input_writer, input_reader) = tokio::io::duplex(4096);
        let (output_reader, output_writer) = tokio::io::duplex(4096);
        let bridge_task = tokio::spawn(bridge(client, input_reader, output_writer));

        input_writer.write_all(b"{\"id\":1}\n").await.unwrap();
        let mut output_reader = BufReader::new(output_reader);
        let mut response = Vec::new();
        tokio::time::timeout(
            Duration::from_secs(1),
            output_reader.read_until(b'\n', &mut response),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(response, b"{\"id\":1,\"result\":{}}\n");

        input_writer.shutdown().await.unwrap();
        bridge_task.await.unwrap().unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn forward_frames_rejects_an_unterminated_oversized_frame_without_writing() {
        let (mut input_writer, input_reader) = tokio::io::duplex(MAX_FRAME_BYTES + 1);
        let (mut output_reader, output_writer) = tokio::io::duplex(128);
        let task = tokio::spawn(forward_frames(input_reader, output_writer));

        input_writer
            .write_all(&vec![b'x'; MAX_FRAME_BYTES + 1])
            .await
            .unwrap();

        let error = tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("oversized frame must fail without waiting for a newline")
            .unwrap()
            .expect_err("oversized frame must return a bounded error");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("65536"));

        let mut output = Vec::new();
        output_reader.read_to_end(&mut output).await.unwrap();
        assert!(output.is_empty(), "oversized frames must not be forwarded");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bridge_keeps_reading_after_half_close_for_a_late_response() {
        const LATE_RESPONSE_DELAY: Duration = Duration::from_millis(300);

        let dir = tempdir().unwrap();
        let socket_path = dir.path().join("api.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let input_closed = Arc::new(Notify::new());
        let release_response = Arc::new(Notify::new());
        let server_input_closed = input_closed.clone();
        let server_release_response = release_response.clone();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read_half, mut write_half) = stream.into_split();
            let mut reader = BufReader::new(read_half);
            let mut request = Vec::new();
            reader.read_until(b'\n', &mut request).await.unwrap();
            assert_eq!(request, b"{\"id\":\"late\"}\n");

            let mut trailing = Vec::new();
            reader.read_to_end(&mut trailing).await.unwrap();
            assert!(trailing.is_empty());
            server_input_closed.notify_one();
            server_release_response.notified().await;

            write_half
                .write_all(b"{\"id\":\"late\",\"result\":{\"done\":true}}\n")
                .await
                .unwrap();
            write_half.shutdown().await.unwrap();
        });

        let client = UnixStream::connect(&socket_path).await.unwrap();
        let (mut input_writer, input_reader) = tokio::io::duplex(4096);
        let (mut output_reader, output_writer) = tokio::io::duplex(4096);
        let bridge_task = tokio::spawn(bridge(client, input_reader, output_writer));

        let input_closed = input_closed.notified();
        input_writer
            .write_all(b"{\"id\":\"late\"}\n")
            .await
            .unwrap();
        input_writer.shutdown().await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), input_closed)
            .await
            .expect("server must observe the half-close");

        // Cross the old server-side 250 ms drain grace after EOF was observed.
        tokio::time::sleep(LATE_RESPONSE_DELAY).await;
        release_response.notify_one();

        let mut output = Vec::new();
        tokio::time::timeout(
            Duration::from_secs(1),
            output_reader.read_to_end(&mut output),
        )
        .await
        .expect("bridge must keep reading after stdin EOF")
        .unwrap();
        assert_eq!(output, b"{\"id\":\"late\",\"result\":{\"done\":true}}\n");

        bridge_task.await.unwrap().unwrap();
        server.await.unwrap();
    }
}
