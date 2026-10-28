#![allow(dead_code)]

use std::fs::File;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// Serves an app exactly like `varynth_api::serve` so handlers see the socket
/// peer address via `ConnectInfo`.
pub async fn serve(app: axum::Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .expect("serve");
    });
    addr
}

pub async fn text_ok(client: &reqwest::Client, url: &str) -> String {
    let mut last = String::new();
    for _ in 0..20 {
        match client.get(url).send().await {
            Ok(response) if response.status().is_success() => {
                return response.text().await.expect("body");
            }
            Ok(response) => last = format!("{}", response.status()),
            Err(error) => last = error.to_string(),
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("GET {url} failed: {last}");
}

pub fn clean(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    !lower.contains("cloudflare") && !lower.contains("workers") && !body.contains("@cf/")
}

pub struct PgServer {
    bin: PathBuf,
    data: PathBuf,
}

impl Drop for PgServer {
    fn drop(&mut self) {
        let mut cmd = Command::new(self.bin.join(program_name("pg_ctl")));
        cmd.arg("-D")
            .arg(&self.data)
            .args(["stop", "-m", "fast", "-w", "-t", "10"])
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let _ = run_bounded(cmd, Duration::from_secs(15), false);
        if let Some(pid) = postmaster_pid(&self.data) {
            kill_pid(pid, true);
        }
    }
}

pub async fn boot_postgres() -> (String, PgServer) {
    // postgresql_embedded setup() runs initdb without --no-locale, then waits
    // on pg_ctl. On Windows that wait has no timeout and does not return after
    // the server is ready, so a Turkish non-ASCII locale hangs the test.
    eprintln!("boot: initdb --no-locale");
    boot_with_c_locale(postgres_bin_dir())
}

fn postgres_bin_dir() -> PathBuf {
    let root = postgresql_embedded::PostgreSQL::default()
        .settings()
        .installation_dir
        .clone();
    let marker = program_name("initdb");
    let mut found = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&root) {
        for entry in entries.flatten() {
            let bin = entry.path().join("bin");
            if bin.join(&marker).is_file() {
                found.push(bin);
            }
        }
    }
    let direct = root.join("bin");
    if direct.join(&marker).is_file() {
        found.push(direct);
    }
    found.sort();
    found.pop().unwrap_or_else(|| {
        panic!("postgres binaries not found under {}", root.display());
    })
}

fn boot_with_c_locale(bin: PathBuf) -> (String, PgServer) {
    let root = std::env::temp_dir().join(format!("varynth-pg-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("pg root");
    let data = root.join("data");
    let password_file = root.join("pw");
    std::fs::write(&password_file, "varynth\n").expect("pw");

    let init_log = root.join("initdb.log");
    let mut init = Command::new(bin.join(program_name("initdb")));
    init.args([
        "--no-locale",
        "--encoding=UTF8",
        "--username=postgres",
        "--auth=password",
        "--pwfile",
    ])
    .arg(&password_file)
    .arg("--pgdata")
    .arg(&data)
    .env("LC_ALL", "C")
    .env("LANG", "C");
    attach_log(&mut init, &init_log);
    match run_bounded(init, Duration::from_secs(60), true) {
        Ok(status) if status.success() => {}
        Ok(status) => panic!("initdb failed ({status}): {}", log_tail(&init_log)),
        Err(error) => panic!("initdb {error}: {}", log_tail(&init_log)),
    }

    let server = PgServer {
        bin: bin.clone(),
        data: data.clone(),
    };
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("port");
    let port = listener.local_addr().expect("addr").port();
    drop(listener);

    eprintln!("boot: pg_ctl start port {port}");
    let start_log = root.join("start.log");
    let ctl_log = root.join("pg_ctl.log");
    let options = format!("-p {port} -h 127.0.0.1");
    let mut start = Command::new(bin.join(program_name("pg_ctl")));
    start
        .arg("-D")
        .arg(&data)
        .arg("-l")
        .arg(&start_log)
        .args(["-W", "-t", "20", "start", "-o"])
        .arg(&options);
    attach_log(&mut start, &ctl_log);
    match run_bounded(start, Duration::from_secs(20), false) {
        Ok(status) if status.success() => {}
        Ok(status) => {
            if !wait_ready(&bin, port, Duration::from_secs(5)) {
                panic!(
                    "pg_ctl failed ({status}): {}\n{}",
                    log_tail(&ctl_log),
                    log_tail(&start_log)
                );
            }
        }
        Err(error) => {
            if !wait_ready(&bin, port, Duration::from_secs(20)) {
                panic!(
                    "pg_ctl {error}: {}\n{}",
                    log_tail(&ctl_log),
                    log_tail(&start_log)
                );
            }
        }
    }
    if !wait_ready(&bin, port, Duration::from_secs(30)) {
        panic!(
            "postgres did not become ready: {}\n{}",
            log_tail(&ctl_log),
            log_tail(&start_log)
        );
    }
    eprintln!("boot: postgres ready");
    (
        format!("postgres://postgres:varynth@127.0.0.1:{port}/postgres"),
        server,
    )
}

fn wait_ready(bin: &Path, port: u16, timeout: Duration) -> bool {
    let program = bin.join(program_name("pg_isready"));
    let started = Instant::now();
    while started.elapsed() < timeout {
        let mut cmd = Command::new(&program);
        cmd.args([
            "-h",
            "127.0.0.1",
            "-p",
            &port.to_string(),
            "-t",
            "2",
            "-U",
            "postgres",
            "-d",
            "postgres",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null());
        if let Ok(status) = run_bounded(cmd, Duration::from_secs(5), true) {
            if status.success() {
                return true;
            }
        }
        thread::sleep(Duration::from_millis(200));
    }
    false
}

fn attach_log(cmd: &mut Command, path: &Path) {
    if let Ok(file) = File::create(path) {
        if let Ok(err) = file.try_clone() {
            cmd.stdout(Stdio::from(file)).stderr(Stdio::from(err));
            return;
        }
    }
    cmd.stdout(Stdio::null()).stderr(Stdio::null());
}

fn run_bounded(
    mut cmd: Command,
    timeout: Duration,
    kill_tree: bool,
) -> Result<std::process::ExitStatus, String> {
    quiet(&mut cmd);
    cmd.stdin(Stdio::null());
    let mut child = cmd.spawn().map_err(|error| error.to_string())?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if started.elapsed() >= timeout => {
                kill_pid(child.id(), kill_tree);
                wait_brief(&mut child);
                return Err(format!("timed out after {}s", timeout.as_secs()));
            }
            Ok(None) => thread::sleep(Duration::from_millis(100)),
            Err(error) => return Err(error.to_string()),
        }
    }
}

fn wait_brief(child: &mut std::process::Child) {
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(2) {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return,
            Ok(None) => thread::sleep(Duration::from_millis(50)),
        }
    }
}

fn postmaster_pid(data: &Path) -> Option<u32> {
    let text = std::fs::read_to_string(data.join("postmaster.pid")).ok()?;
    let pid = text.lines().next()?.trim().parse().ok()?;
    if pid == 0 { None } else { Some(pid) }
}

fn kill_pid(pid: u32, tree: bool) {
    if pid == 0 {
        return;
    }
    #[cfg(windows)]
    {
        let mut args = vec!["/F"];
        if tree {
            args.push("/T");
        }
        let pid_text = pid.to_string();
        args.push("/PID");
        args.push(&pid_text);
        let mut cmd = Command::new("taskkill");
        cmd.args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        quiet(&mut cmd);
        let _ = cmd.status();
    }
    #[cfg(not(windows))]
    {
        let _ = tree;
        let _ = Command::new("kill")
            .args(["-9", &pid.to_string()])
            .status();
    }
}

fn program_name(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

#[cfg(windows)]
fn quiet(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn quiet(_cmd: &mut Command) {}

fn log_tail(path: &Path) -> String {
    let Ok(text) = std::fs::read_to_string(path) else {
        return "(no log)".into();
    };
    let mut start = text.len().saturating_sub(2000);
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].to_string()
}
