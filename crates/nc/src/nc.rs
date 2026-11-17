use anyhow::{Context as _, Result};
use std::io::{Read, Write};
use std::sync::mpsc;
use std::thread;

/// The main function for when Zed is running in netcat mode: it connects
/// standard input and output to the Unix socket at `socket`.
///
/// This uses blocking threads rather than an async executor because async
/// standard input and output are not available on Windows.
pub fn main(socket: &str) -> Result<()> {
    let stream = net::UnixStream::connect(socket)
        .with_context(|| format!("connecting to socket {socket}"))?;
    #[cfg(not(target_os = "windows"))]
    let (socket_read, socket_write) = (stream.try_clone()?, stream);
    #[cfg(target_os = "windows")]
    let (socket_read, socket_write) = stream.into_split();
    bridge(
        socket_read,
        socket_write,
        std::io::stdin(),
        std::io::stdout(),
    )
}

/// Copies `input` to the socket and the socket to `output` until either side
/// closes.
fn bridge(
    socket_read: impl Read + Send + 'static,
    socket_write: impl Write + Send + 'static,
    input: impl Read + Send + 'static,
    output: impl Write + Send + 'static,
) -> Result<()> {
    let (finished_sender, finished_receiver) = mpsc::channel();
    thread::spawn({
        let finished_sender = finished_sender.clone();
        move || {
            // The receiver is gone only when the other direction already finished.
            finished_sender.send(copy(input, socket_write)).ok();
        }
    });
    thread::spawn(move || {
        finished_sender.send(copy(socket_read, output)).ok();
    });
    // The thread for the other direction may still be blocked on a read. It is
    // left behind because the process exits as soon as this returns.
    finished_receiver.recv()?
}

fn copy(mut reader: impl Read, mut writer: impl Write) -> Result<()> {
    let mut buffer = [0; 8192];
    loop {
        let bytes_read = reader.read(&mut buffer)?;
        if bytes_read == 0 {
            return Ok(());
        }
        let chunk = buffer.get(..bytes_read).context("read past buffer")?;
        writer.write_all(chunk)?;
        writer.flush()?;
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    #[test]
    fn test_bridge_copies_both_directions_until_input_closes() -> Result<()> {
        let (socket, mut peer) = UnixStream::pair()?;
        let (input, mut input_writer) = UnixStream::pair()?;
        let (output, mut output_reader) = UnixStream::pair()?;
        let socket_read = socket.try_clone()?;
        let bridge_thread = thread::spawn(move || bridge(socket_read, socket, input, output));

        input_writer.write_all(b"request\n")?;
        let mut request = [0; 8];
        peer.read_exact(&mut request)?;
        assert_eq!(&request, b"request\n");

        peer.write_all(b"response\n")?;
        let mut response = [0; 9];
        output_reader.read_exact(&mut response)?;
        assert_eq!(&response, b"response\n");

        drop(input_writer);
        bridge_thread
            .join()
            .map_err(|_| anyhow::anyhow!("bridge thread panicked"))?
    }
}
