use portable_pty::{native_pty_system, Child, ChildKiller, CommandBuilder, MasterPty, PtySize};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::{env, fs, process, thread};

const SCROLLBACK: usize = 5000;
const QUEUE: usize = 4096;

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
enum Req {
    Spawn { name: String, cwd: String, argv: Vec<String>, env: HashMap<String, String>, cols: u16, rows: u16 },
    List,
    Kill { name: String },
    State { name: String, value: String },
    Attach { name: String },
}

#[derive(Default)]
struct Hooks {
    title: String,
    replies: Vec<u8>,
}

impl vt100::Callbacks for Hooks {
    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.title = String::from_utf8_lossy(title).into_owned();
    }

    fn unhandled_csi(&mut self, screen: &mut vt100::Screen, i1: Option<u8>, _: Option<u8>, params: &[&[u16]], c: char) {
        let params: Vec<u16> = params.iter().flat_map(|p| p.iter().copied()).collect();
        match (i1, c, params.as_slice()) {
            (None, 'n', [5]) => self.replies.extend_from_slice(b"\x1b[0n"),
            (None, 'n', [6]) => {
                let (row, col) = screen.cursor_position();
                self.replies.extend(format!("\x1b[{};{}R", row + 1, col + 1).bytes());
            }
            (None, 'c', [] | [0]) => self.replies.extend_from_slice(b"\x1b[?1;2c"),
            _ => {}
        }
    }
}

struct Term {
    parser: vt100::Parser<Hooks>,
    clients: Vec<(u64, SyncSender<Vec<u8>>)>,
    primary: u64,
    state: String,
}

struct Session {
    pid: u32,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    term: Mutex<Term>,
}

struct Server {
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    info: PathBuf,
    port: u16,
    token: String,
}

static NEXT_CLIENT: AtomicU64 = AtomicU64::new(1);

fn frame(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 5);
    out.push(kind);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

fn hangup(session: &Session) {
    #[cfg(unix)]
    if session.pid > 0 {
        unsafe { libc::kill(-(session.pid as i32), libc::SIGHUP) };
    }
    let _ = session.killer.lock().unwrap().kill();
}

fn is_reply(input: &[u8]) -> bool {
    (input.len() > 3 && input.starts_with(b"\x1b[") && matches!(input.last(), Some(b'R' | b'c' | b'n')))
        || (input.len() > 2 && (input.starts_with(b"\x1b]") || input.starts_with(b"\x1bP")))
}

fn snapshot<CB: vt100::Callbacks>(parser: &mut vt100::Parser<CB>) -> Vec<u8> {
    let mut out = b"\x1bc".to_vec();
    let screen = parser.screen_mut();
    let (rows, cols) = screen.size();
    screen.set_scrollback(usize::MAX);
    let total = screen.scrollback();
    let mut offset = total;
    let mut first = true;
    while offset > 0 {
        screen.set_scrollback(offset);
        let count = offset.min(usize::from(rows));
        let mut wrapped = false;
        let lines: Vec<Vec<u8>> = screen.rows_formatted(0, cols).take(count).collect();
        for (i, line) in lines.into_iter().enumerate() {
            if !first && !wrapped {
                out.extend_from_slice(b"\r\n");
            }
            out.extend_from_slice(b"\x1b[m");
            out.extend(line);
            wrapped = screen.row_wrapped(i as u16);
            first = false;
        }
        offset -= count;
    }
    screen.set_scrollback(0);
    if total > 0 {
        out.extend(b"\r\n".repeat(usize::from(rows)));
    }
    if screen.alternate_screen() {
        out.extend_from_slice(b"\x1b[?1049h");
    }
    out.extend(screen.state_formatted());
    out
}

fn spawn(server: &Arc<Server>, name: String, cwd: String, argv: Vec<String>, vars: HashMap<String, String>, cols: u16, rows: u16) -> Result<Value, String> {
    let program = argv.first().ok_or("empty argv")?;
    let pair = native_pty_system()
        .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
        .map_err(|e| e.to_string())?;
    let mut cmd = CommandBuilder::new(program);
    cmd.args(&argv[1..]);
    cmd.cwd(cwd);
    cmd.env_clear();
    for (key, value) in vars {
        cmd.env(key, value);
    }
    cmd.env("LITEMUX", &server.info);
    cmd.env("LITEMUX_SESSION", &name);
    cmd.env("LITEMUX_BIN", env::current_exe().map_err(|e| e.to_string())?);
    let child = pair.slave.spawn_command(cmd).map_err(|e| e.to_string())?;
    drop(pair.slave);
    let reader = pair.master.try_clone_reader().map_err(|e| e.to_string())?;
    let writer = pair.master.take_writer().map_err(|e| e.to_string())?;
    let session = Arc::new(Session {
        pid: child.process_id().unwrap_or(0),
        killer: Mutex::new(child.clone_killer()),
        master: Mutex::new(pair.master),
        writer: Mutex::new(writer),
        term: Mutex::new(Term {
            parser: vt100::Parser::new_with_callbacks(rows, cols, SCROLLBACK, Hooks::default()),
            clients: Vec::new(),
            primary: 0,
            state: String::new(),
        }),
    });
    if let Some(old) = server.sessions.lock().unwrap().insert(name.clone(), session.clone()) {
        hangup(&old);
    }
    let (done_tx, done_rx) = sync_channel(1);
    let pumped = session.clone();
    thread::spawn(move || pump(pumped, reader, done_tx));
    let server = server.clone();
    thread::spawn(move || reap(server, name, session, child, done_rx));
    Ok(json!({ "ok": true }))
}

fn pump(session: Arc<Session>, mut reader: Box<dyn Read + Send>, done: SyncSender<()>) {
    let mut buf = vec![0u8; 65536];
    while let Ok(n) = reader.read(&mut buf) {
        if n == 0 {
            break;
        }
        let mut term = session.term.lock().unwrap();
        term.parser.process(&buf[..n]);
        let replies = std::mem::take(&mut term.parser.callbacks_mut().replies);
        let data = frame(b'D', &buf[..n]);
        term.clients.retain(|(_, tx)| tx.try_send(data.clone()).is_ok());
        let answer = term.clients.is_empty() && !replies.is_empty();
        drop(term);
        if answer {
            let _ = session.writer.lock().unwrap().write_all(&replies);
        }
    }
    let _ = done.send(());
}

fn reap(server: Arc<Server>, name: String, session: Arc<Session>, mut child: Box<dyn Child + Send + Sync>, done: Receiver<()>) {
    let code = child.wait().map(|s| i64::from(s.exit_code())).unwrap_or(-1);
    let _ = done.recv_timeout(Duration::from_millis(500));
    hangup(&session);
    let mut map = server.sessions.lock().unwrap();
    if map.get(&name).is_some_and(|s| Arc::ptr_eq(s, &session)) {
        map.remove(&name);
    }
    let exit = frame(b'X', code.to_string().as_bytes());
    for (_, tx) in session.term.lock().unwrap().clients.drain(..) {
        let _ = tx.try_send(exit.clone());
    }
    if map.is_empty() {
        thread::sleep(Duration::from_millis(100));
        if read_info(&server.info).is_ok_and(|(port, _)| port == server.port) {
            let _ = fs::remove_file(&server.info);
        }
        process::exit(0);
    }
}

fn handle(req: Req, server: &Arc<Server>) -> Result<Value, String> {
    match req {
        Req::Spawn { name, cwd, argv, env, cols, rows } => spawn(server, name, cwd, argv, env, cols, rows),
        Req::List => {
            let map = server.sessions.lock().unwrap();
            let list: Vec<Value> = map
                .iter()
                .map(|(name, s)| {
                    let term = s.term.lock().unwrap();
                    json!({ "name": name, "pid": s.pid, "title": term.parser.callbacks().title, "state": term.state })
                })
                .collect();
            Ok(json!({ "ok": true, "sessions": list }))
        }
        Req::Kill { name } => {
            if let Some(s) = server.sessions.lock().unwrap().get(&name) {
                hangup(s);
            }
            Ok(json!({ "ok": true }))
        }
        Req::State { name, value } => {
            let map = server.sessions.lock().unwrap();
            let s = map.get(&name).ok_or("no such session")?;
            s.term.lock().unwrap().state = value;
            Ok(json!({ "ok": true }))
        }
        Req::Attach { .. } => Err("attach needs a stream".into()),
    }
}

fn feed(mut stream: TcpStream, rx: Receiver<Vec<u8>>) {
    for buf in rx {
        if stream.write_all(&buf).is_err() {
            break;
        }
    }
    let _ = stream.shutdown(Shutdown::Both);
}

fn attach(mut out: TcpStream, mut reader: BufReader<TcpStream>, session: Arc<Session>) {
    let id = NEXT_CLIENT.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = sync_channel(QUEUE);
    if writeln!(out, "{}", json!({ "ok": true })).is_err() {
        return;
    }
    {
        let mut term = session.term.lock().unwrap();
        let _ = tx.try_send(frame(b'D', &snapshot(&mut term.parser)));
        term.clients.push((id, tx));
        term.primary = id;
    }
    thread::spawn(move || feed(out, rx));
    let mut head = [0u8; 5];
    while reader.read_exact(&mut head).is_ok() {
        let mut body = vec![0u8; u32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize];
        if reader.read_exact(&mut body).is_err() {
            break;
        }
        match head[0] {
            b'I' => {
                let mut term = session.term.lock().unwrap();
                if is_reply(&body) && term.primary != id {
                    continue;
                }
                if !is_reply(&body) {
                    term.primary = id;
                }
                drop(term);
                let _ = session.writer.lock().unwrap().write_all(&body);
            }
            b'R' if body.len() == 4 => {
                let cols = u16::from_be_bytes([body[0], body[1]]);
                let rows = u16::from_be_bytes([body[2], body[3]]);
                if cols > 1 && rows > 1 {
                    let _ = session.master.lock().unwrap().resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
                    session.term.lock().unwrap().parser.screen_mut().set_size(rows, cols);
                }
            }
            _ => {}
        }
    }
    let mut term = session.term.lock().unwrap();
    term.clients.retain(|(client, _)| *client != id);
    if term.primary == id {
        term.primary = term.clients.last().map_or(0, |(client, _)| *client);
    }
}

fn serve(stream: TcpStream, server: Arc<Server>) {
    let Ok(clone) = stream.try_clone() else { return };
    let mut reader = BufReader::new(clone);
    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let mut out = stream;
    let result = match parse(&line, &server.token) {
        Ok(Req::Attach { name }) => {
            let session = server.sessions.lock().unwrap().get(&name).cloned();
            match session {
                Some(session) => return attach(out, reader, session),
                None => Err("no such session".to_string()),
            }
        }
        Ok(req) => handle(req, &server),
        Err(e) => Err(e),
    };
    let reply = result.unwrap_or_else(|e| json!({ "ok": false, "error": e }));
    let _ = writeln!(out, "{reply}");
}

fn parse(line: &str, token: &str) -> Result<Req, String> {
    let mut value: Value = serde_json::from_str(line).map_err(|e| e.to_string())?;
    let given = value.as_object_mut().and_then(|o| o.remove("token"));
    if given.as_ref().and_then(Value::as_str) != Some(token) {
        return Err("bad token".into());
    }
    serde_json::from_value(value).map_err(|e| e.to_string())
}

fn new_token() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| e.to_string())?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn read_info(info: &Path) -> Result<(u16, String), String> {
    let text = fs::read_to_string(info).map_err(|e| e.to_string())?;
    let value: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    let port = value["port"].as_u64().and_then(|p| u16::try_from(p).ok()).ok_or("bad info file")?;
    Ok((port, value["token"].as_str().unwrap_or("").to_string()))
}

fn write_info(info: &Path, port: u16, token: &str) -> std::io::Result<()> {
    let temporary = info.with_extension("tmp");
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(&temporary)?.write_all(json!({ "port": port, "token": token, "pid": process::id() }).to_string().as_bytes())?;
    fs::rename(temporary, info)
}

fn server(info: PathBuf) -> Result<(), String> {
    if request(&info, json!({ "op": "list" })).is_ok() {
        return Ok(());
    }
    let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(|e| e.to_string())?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let token = new_token()?;
    write_info(&info, port, &token).map_err(|e| e.to_string())?;
    let server = Arc::new(Server { sessions: Mutex::default(), info, port, token });
    for stream in listener.incoming().flatten() {
        let _ = stream.set_nodelay(true);
        let server = server.clone();
        thread::spawn(move || serve(stream, server));
    }
    Ok(())
}

fn request(info: &Path, mut req: Value) -> Result<Value, String> {
    let (port, token) = read_info(info)?;
    req["token"] = Value::String(token);
    let mut stream = TcpStream::connect(("127.0.0.1", port)).map_err(|e| e.to_string())?;
    writeln!(stream, "{req}").map_err(|e| e.to_string())?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).map_err(|e| e.to_string())?;
    let reply: Value = serde_json::from_str(&line).map_err(|e| e.to_string())?;
    if reply["ok"] == true {
        Ok(reply)
    } else {
        Err(reply["error"].as_str().unwrap_or("request failed").to_string())
    }
}

fn run(mut args: Vec<String>) -> Result<(), String> {
    let mut info = env::var("LITEMUX").ok();
    if args.first().map(String::as_str) == Some("-S") && args.len() > 1 {
        info = Some(args[1].clone());
        args.drain(..2);
    }
    let info = PathBuf::from(info.ok_or("no server file: pass -S FILE or set LITEMUX")?);
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["server"] => server(info),
        ["state", value] => {
            let name = env::var("LITEMUX_SESSION").map_err(|_| "LITEMUX_SESSION is not set")?;
            request(&info, json!({ "op": "state", "name": name, "value": value })).map(|_| ())
        }
        ["ls"] => {
            for s in request(&info, json!({ "op": "list" }))?["sessions"].as_array().into_iter().flatten() {
                println!("{}\t{}\t{}\t{}", s["name"].as_str().unwrap_or(""), s["pid"], s["state"].as_str().unwrap_or(""), s["title"].as_str().unwrap_or(""));
            }
            Ok(())
        }
        ["kill", name] => request(&info, json!({ "op": "kill", "name": name })).map(|_| ()),
        _ => Err("usage: litemux [-S file] server | ls | kill NAME | state VALUE".into()),
    }
}

fn main() {
    if let Err(e) = run(env::args().skip(1).collect()) {
        eprintln!("litemux: {e}");
        process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn history(parser: &mut vt100::Parser) -> Vec<String> {
        let screen = parser.screen_mut();
        screen.set_scrollback(usize::MAX);
        let total = screen.scrollback();
        let rows = (0..=total)
            .map(|offset| {
                screen.set_scrollback(offset);
                screen.contents()
            })
            .collect();
        screen.set_scrollback(0);
        rows
    }

    #[test]
    fn snapshot_restores_screen_and_scrollback() {
        let mut source = vt100::Parser::new(5, 20, 100);
        for i in 0..30 {
            source.process(format!("\x1b[3{}mline {i}\x1b[m\r\n", i % 7).as_bytes());
        }
        source.process(b"a long line that wraps past twenty columns\r\n$ ");
        let mut restored = vt100::Parser::new(5, 20, 100);
        restored.process(&snapshot(&mut source));
        assert_eq!(restored.screen().contents_formatted(), source.screen().contents_formatted());
        assert_eq!(restored.screen().cursor_position(), source.screen().cursor_position());
        assert_eq!(history(&mut restored), history(&mut source));
    }

    #[test]
    fn snapshot_restores_alternate_screen_and_modes() {
        let mut source = vt100::Parser::new(5, 20, 100);
        source.process(b"shell\r\n\x1b[?1049h\x1b[?2004h\x1b[?1000h\x1b[2;3Hfull screen");
        let mut restored = vt100::Parser::new(5, 20, 100);
        restored.process(&snapshot(&mut source));
        assert!(restored.screen().alternate_screen());
        assert!(restored.screen().bracketed_paste());
        assert_eq!(restored.screen().contents(), source.screen().contents());
        assert_eq!(restored.screen().cursor_position(), (1, 13));
    }

    #[test]
    fn rejects_requests_without_the_token() {
        assert!(matches!(parse(r#"{"op":"list","token":"abc"}"#, "abc"), Ok(Req::List)));
        assert_eq!(parse(r#"{"op":"list","token":"nope"}"#, "abc").err().as_deref(), Some("bad token"));
        assert_eq!(parse(r#"{"op":"list"}"#, "abc").err().as_deref(), Some("bad token"));
    }

    #[test]
    fn recognises_terminal_replies() {
        assert!(is_reply(b"\x1b[12;40R"));
        assert!(is_reply(b"\x1b[?1;2c"));
        assert!(is_reply(b"\x1b]11;rgb:0000/0000/0000\x1b\\"));
        assert!(!is_reply(b"\x1b[A"));
        assert!(!is_reply(b"hello"));
        assert!(!is_reply(b"\x1b]"));
    }
}
