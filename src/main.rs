#[macro_use]
extern crate failure;
extern crate notify;
extern crate percent_encoding;
#[macro_use]
extern crate log;
extern crate env_logger;

use failure::Error;
use notify::{DebouncedEvent, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::HashMap;
use std::io::stdin;
use std::process::exit;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::thread;
use std::time::Duration;

type Result<R> = std::result::Result<R, Error>;

fn encode(s: &str) -> impl AsRef<str> {
    percent_encoding::utf8_percent_encode(s, percent_encoding::SIMPLE_ENCODE_SET).to_string()
}

fn decode<'a>(s: &'a str) -> impl AsRef<str> + 'a {
    percent_encoding::percent_decode(s.as_bytes()).decode_utf8_lossy()
}

fn send_cmd(cmd: &str, args: &[&str]) {
    debug!("output: {} {:?}", cmd, args);

    let mut output = cmd.to_owned();
    for arg in args {
        output += " ";
        output += &encode(arg).as_ref();
    }
    println!("{}", output);
}

fn ack() {
    send_cmd("OK", &[]);
}

fn changes(replica: &str) {
    send_cmd("CHANGES", &[replica]);
}

fn recursive(path: &str) {
    send_cmd("RECURSIVE", &[path]);
}

fn done() {
    send_cmd("DONE", &[]);
}

fn error(msg: &str) {
    send_cmd("ERROR", &[msg]);
    exit(1);
}

fn parse_input(input: &str) -> Result<(String, Vec<String>)> {
    debug!("input: {}", input);

    // TODO: Handle EOF
    let mut cmd = String::new();
    let mut args = vec![];
    for (idx, word) in input.split_whitespace().enumerate() {
        if idx == 0 {
            cmd = word.to_owned();
        } else {
            args.push(decode(word).as_ref().to_owned())
        }
    }
    Ok((cmd, args))
}

fn add_to_watcher(
    watcher: &mut RecommendedWatcher,
    fspath: &str,
    rx: &Receiver<String>,
) -> Result<()> {
    watcher.watch(fspath, RecursiveMode::Recursive)?;
    ack();

    loop {
        let input = rx.recv()?;
        let (cmd, _) = parse_input(&input)?;
        match cmd.as_str() {
            "DIR" => ack(),
            "LINK" => bail!("link following is not supported, please disable this option (-links)"),
            "DONE" => break,
            _ => error(&format!("Unexpected cmd: {}", cmd)),
        }
    }

    Ok(())
}

fn handle_fsevent(
    rx: &Receiver<DebouncedEvent>,
    replicas: &HashMap<String, String>,
    pending_changes: &mut HashMap<String, Vec<String>>,
) -> Result<()> {
    for event in rx.try_iter() {
        debug!("FS event: {:?}", event);

        let mut paths = vec![];
        match event {
            DebouncedEvent::NoticeWrite(path)
            | DebouncedEvent::NoticeRemove(path)
            | DebouncedEvent::Create(path)
            | DebouncedEvent::Write(path)
            | DebouncedEvent::Chmod(path)
            | DebouncedEvent::Remove(path) => paths.push(path),
            DebouncedEvent::Rename(path1, path2) => {
                paths.push(path1);
                paths.push(path2);
            }
            DebouncedEvent::Error(err, path) => {
                bail!("Error occured at watched path ({:?}): {}", path, err);
            }
            _ => {}
        }

        for file_path in paths {
            for (replica, replica_path) in replicas {
                if file_path.starts_with(replica_path) {
                    let relative_path = file_path.strip_prefix(replica_path)?;
                    pending_changes
                        .entry(replica.clone())
                        .or_default()
                        .push(relative_path.to_string_lossy().into());
                }
            }
        }
    }

    for replica in pending_changes.keys() {
        changes(replica);
    }

    Ok(())
}

fn main() -> Result<()> {
    env_logger::init();

    send_cmd("VERSION", &["1"]);

    let (stdin_tx, stdin_rx) = channel();
    thread::spawn(move || loop {
        let mut input = String::new();
        stdin().read_line(&mut input).unwrap();
        stdin_tx.send(input).unwrap();
    });

    let input = stdin_rx.recv()?;
    let (cmd, args) = parse_input(&input)?;
    if cmd != "VERSION" {
        bail!("Unexpected version cmd: {}", cmd);
    }
    let version = args.get(0);
    if version != Some(&"1".to_owned()) {
        bail!("Unexpected version: {:?}", version);
    }

    // id => path.
    let mut replicas = HashMap::new();

    // id => changed paths.
    let mut pending_changes = HashMap::new();

    let delay = 1;
    let (fsevent_tx, fsevent_rx) = channel();
    let mut watcher: RecommendedWatcher = Watcher::new(fsevent_tx, Duration::from_secs(delay))?;

    loop {
        handle_fsevent(&fsevent_rx, &replicas, &mut pending_changes)?;

        let input = match stdin_rx.recv_timeout(Duration::from_secs(1)) {
            Ok(input) => input,
            Err(RecvTimeoutError::Timeout) => {
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => {
                break;
            }
        };

        if input.is_empty() {
            break;
        }

        let (cmd, mut args) = parse_input(&input)?;

        if cmd != "WAIT" {
            pending_changes.clear();
        }

        if cmd == "DEBUG" {
        } else if cmd == "START" {
            // Start observing replica.
            let replica = args.remove(0);
            let path = args.remove(0);
            add_to_watcher(&mut watcher, &path, &stdin_rx)?;
            replicas.insert(replica, path);
        } else if cmd == "WAIT" {
            // Start waiting replica.
        } else if cmd == "CHANGES" {
            // Request pending replicas.
            let replica = args.remove(0);
            let replica_changes: Vec<String> = pending_changes.remove(&replica).unwrap_or_default();
            for c in replica_changes {
                recursive(&c);
            }
            done();
        } else if cmd == "RESET" {
            // Stop observing replica.
            let replica = args.remove(0);
            watcher.unwatch(replica)?;
        } else {
            error(&format!("Unexpected root cmd: {}", cmd));
        }
    }

    Ok(())
}
