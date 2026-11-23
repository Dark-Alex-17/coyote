use super::*;
use std::io::{self, Read};
#[cfg(unix)]
use std::process::{Child, ChildStdout, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

pub(super) const WAIT: Duration = Duration::from_secs(10);

/// Reads one line the way bash's `read` builtin does: a byte at a
/// time, stopping at the newline, without pulling in what follows.
pub(super) fn read_line_bytewise(reader: &mut impl Read) -> io::Result<Option<String>> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if reader.read(&mut byte)? == 0 {
            break;
        }
        let byte = byte[0];
        line.push(byte);
        if byte == b'\n' {
            break;
        }
    }
    if line.is_empty() {
        return Ok(None);
    }
    Ok(Some(String::from_utf8(line).unwrap()))
}

pub(super) fn reply_line(id: &str, answer: &Value) -> String {
    let mut line = json!({"id": id, "answer": answer}).to_string();
    line.push('\n');
    line
}

fn read_line_on_a_thread(
    mut client: impl Read + Send + 'static,
) -> mpsc::Receiver<io::Result<Option<String>>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || tx.send(read_line_bytewise(&mut client)).unwrap());
    rx
}

/// A client on the channel: `head` spawned the way a tool is, reading
/// one line off the inherited descriptor and handing it on through
/// its stdout.
#[cfg(unix)]
fn open_client(server: &mut Server) -> io::Result<impl Read + Send + use<>> {
    let mut cmd = Command::new("head");
    cmd.args(["-n1", &server.env_value()])
        .stdin(Stdio::null())
        .stdout(Stdio::piped());
    server.configure(&mut cmd);
    let mut child = cmd.spawn()?;
    server.spawned();
    let stdout = child.stdout.take().unwrap();
    Ok(ClientChild { child, stdout })
}

#[cfg(unix)]
struct ClientChild {
    child: Child,
    stdout: ChildStdout,
}

#[cfg(unix)]
impl Read for ClientChild {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.stdout.read(buf)
    }
}

/// Killed rather than waited for: a test that fails before the line
/// arrives would otherwise wait on a child blocked on the pipe.
#[cfg(unix)]
impl Drop for ClientChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A client on the channel: the pipe opened by the path a tool gets.
#[cfg(windows)]
fn open_client(server: &mut Server) -> io::Result<impl Read + Send + use<>> {
    std::fs::File::open(server.env_value())
}

#[test]
fn a_connected_client_reads_the_reply_as_one_json_line() {
    let mut server = Server::open().unwrap();
    let rx = read_line_on_a_thread(open_client(&mut server).unwrap());
    // Let the client connect first so the reply meets a waiting
    // reader rather than the queue.
    thread::sleep(Duration::from_millis(200));
    server.reply("p1", &json!(true));
    let line = rx.recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(line, Some(reply_line("p1", &json!(true))));
}

#[test]
fn a_reply_queued_before_the_client_opens_is_delivered() {
    let mut server = Server::open().unwrap();
    server.reply("p2", &json!("yes"));
    let rx = read_line_on_a_thread(open_client(&mut server).unwrap());
    let line = rx.recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(line, Some(reply_line("p2", &json!("yes"))));
}

/// A client blocked in a read sees the channel close when the server
/// is dropped, so the helper reports "reply channel closed" rather
/// than waiting forever.
#[test]
fn dropping_the_server_releases_a_client_blocked_on_a_read() {
    let mut server = Server::open().unwrap();
    let rx = read_line_on_a_thread(open_client(&mut server).unwrap());
    thread::sleep(Duration::from_millis(200));
    drop(server);
    let result = rx.recv_timeout(WAIT).unwrap();
    assert!(
        matches!(result, Ok(None) | Err(_)),
        "client should see end of stream, got {result:?}"
    );
}
