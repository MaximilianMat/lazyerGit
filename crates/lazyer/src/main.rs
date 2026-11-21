//! `lazyer daemon`: a long-lived process that answers git command lines from
//! the engine, over a Unix socket. `lazyer exec` is a small client for it.
//!
//! Protocol, one request and one reply per frame:
//!
//!   request:  u32 length (big endian), then `cwd \0 arg \0 arg ...`
//!   reply:    u8 handled (0 or 1), i32 exit code, u32 length, stdout bytes
//!
//! "Not handled" means the caller runs the command with real git.

use anyhow::{bail, Context, Result};
use lazyer_core::Native;
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const VERSION: &str = env!("CARGO_PKG_VERSION");
/// The daemon ends itself after this long without a request.
const IDLE_EXIT: Duration = Duration::from_secs(8 * 3600);

fn socket_path() -> PathBuf {
    if let Some(p) = std::env::var_os("LAZYER_SOCKET") {
        return PathBuf::from(p);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    home.join(".cache").join("lazyer").join("daemon.sock")
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("daemon") => daemon(),
        Some("exec") => {
            // lazyer exec [-C dir] -- <git arguments>
            let mut cwd = std::env::current_dir()?;
            let mut rest = &args[1..];
            if rest.first().map(String::as_str) == Some("-C") {
                cwd = PathBuf::from(rest.get(1).context("-C needs a directory")?);
                rest = &rest[2..];
            }
            if rest.first().map(String::as_str) == Some("--") {
                rest = &rest[1..];
            }
            let cwd = cwd.canonicalize()?;
            match request(&cwd, rest, true)? {
                Some((code, stdout)) => {
                    std::io::stdout().write_all(&stdout)?;
                    std::process::exit(code);
                }
                None => {
                    eprintln!("lazyer: not handled, run it with git");
                    std::process::exit(125);
                }
            }
        }
        Some("stop") => {
            let _ = request(Path::new("/"), &["lazyer-shutdown".to_string()], false);
            Ok(())
        }
        Some("status") => match request(Path::new("/"), &["lazyer-ping".to_string()], false) {
            Ok(Some((_, out))) => {
                println!("{}", String::from_utf8_lossy(&out));
                Ok(())
            }
            _ => bail!("no daemon at {}", socket_path().display()),
        },
        _ => bail!("usage: lazyer daemon | exec [-C dir] -- <git args> | status | stop"),
    }
}

// ------------------------------------------------------------------ client

fn connect(spawn: bool) -> Result<UnixStream> {
    let path = socket_path();
    if let Ok(s) = UnixStream::connect(&path) {
        return Ok(s);
    }
    if !spawn {
        bail!("no daemon");
    }
    use std::os::unix::process::CommandExt;
    let log = path.with_extension("log");
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let log_file = std::fs::OpenOptions::new().create(true).append(true).open(&log)?;
    std::process::Command::new(std::env::current_exe()?)
        .arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(log_file)
        .process_group(0)
        .spawn()
        .context("starting the daemon")?;
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(3) {
        if let Ok(s) = UnixStream::connect(&path) {
            return Ok(s);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    bail!("the daemon did not come up; see {}", log.display())
}

fn request(cwd: &Path, argv: &[String], spawn: bool) -> Result<Option<(i32, Vec<u8>)>> {
    let mut s = connect(spawn)?;
    let mut body = cwd.to_string_lossy().as_bytes().to_vec();
    for a in argv {
        body.push(0);
        body.extend_from_slice(a.as_bytes());
    }
    s.write_all(&(body.len() as u32).to_be_bytes())?;
    s.write_all(&body)?;
    let mut head = [0u8; 9];
    s.read_exact(&mut head)?;
    let len = u32::from_be_bytes(head[5..9].try_into().unwrap()) as usize;
    let mut out = vec![0u8; len];
    s.read_exact(&mut out)?;
    Ok((head[0] == 1).then(|| (i32::from_be_bytes(head[1..5].try_into().unwrap()), out)))
}

// ------------------------------------------------------------------ daemon

struct State {
    native: Native,
    started: Instant,
    last_request_ms: AtomicU64,
    handled: AtomicU64,
    declined: AtomicU64,
}

fn daemon() -> Result<()> {
    let path = socket_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if UnixStream::connect(&path).is_ok() {
        bail!("a daemon is already listening on {}", path.display());
    }
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).with_context(|| format!("binding {}", path.display()))?;
    let state = Arc::new(State {
        native: Native::new(),
        started: Instant::now(),
        last_request_ms: AtomicU64::new(0),
        handled: AtomicU64::new(0),
        declined: AtomicU64::new(0),
    });

    {
        let (state, path) = (state.clone(), path.clone());
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_secs(60));
            let idle = state.started.elapsed().as_millis() as u64 - state.last_request_ms.load(Ordering::Relaxed);
            if idle > IDLE_EXIT.as_millis() as u64 {
                let _ = std::fs::remove_file(&path);
                std::process::exit(0);
            }
        });
    }

    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let state = state.clone();
        let path = path.clone();
        std::thread::spawn(move || {
            let _ = serve(stream, &state, &path);
        });
    }
    Ok(())
}

fn serve(mut s: UnixStream, state: &State, socket: &Path) -> Result<()> {
    loop {
        let mut len = [0u8; 4];
        if s.read_exact(&mut len).is_err() {
            return Ok(());
        }
        let mut body = vec![0u8; u32::from_be_bytes(len) as usize];
        s.read_exact(&mut body)?;
        let mut parts = body.split(|b| *b == 0).map(|p| String::from_utf8_lossy(p).to_string());
        let cwd = PathBuf::from(parts.next().unwrap_or_default());
        let argv: Vec<String> = parts.collect();
        state.last_request_ms.store(state.started.elapsed().as_millis() as u64, Ordering::Relaxed);

        let started = Instant::now();
        let reply = match argv.first().map(String::as_str) {
            Some("lazyer-ping") => Some((
                0,
                format!(
                    "lazyer {VERSION}, up {}s, answered {}, passed on {}",
                    state.started.elapsed().as_secs(),
                    state.handled.load(Ordering::Relaxed),
                    state.declined.load(Ordering::Relaxed)
                )
                .into_bytes(),
            )),
            Some("lazyer-shutdown") => {
                let _ = std::fs::remove_file(socket);
                std::process::exit(0);
            }
            _ => state.native.emulate(&cwd, &argv).map(|e| (e.code, e.stdout)),
        };
        if std::env::var_os("LAZYER_TRACE").is_some() {
            let shown: Vec<String> = argv.iter().map(|a| a.chars().take(48).collect()).collect();
            eprintln!(
                "{:>8.2} ms {} git {}",
                started.elapsed().as_secs_f64() * 1000.0,
                if reply.is_some() { "answered " } else { "passed on" },
                shown.join(" ")
            );
        }
        match &reply {
            Some(_) => state.handled.fetch_add(1, Ordering::Relaxed),
            None => state.declined.fetch_add(1, Ordering::Relaxed),
        };
        let (handled, code, out) = match reply {
            Some((code, out)) => (1u8, code, out),
            None => (0u8, 0, Vec::new()),
        };
        s.write_all(&[handled])?;
        s.write_all(&code.to_be_bytes())?;
        s.write_all(&(out.len() as u32).to_be_bytes())?;
        s.write_all(&out)?;
    }
}
