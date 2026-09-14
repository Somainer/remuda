//! Process-lifecycle guarantees for the spawned `fake-herdr server`:
//!
//! * dropping [`FakeHerdrServer`] kills and reaps the child within 2 s;
//! * an idle server does not spin (< 5 % of a core over 1 s, Linux only);
//! * a server whose parent is SIGKILL'd exits by itself (unix only).
//!
//! These guard against the incident where aborted gates left dozens of
//! orphaned servers, each busy-looping at ~100 % CPU.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use remuda_testing::{FakeHerdrOptions, FakeHerdrServer, fake_herdr_bin};

/// `kill -0 <pid>` succeeds iff the process is alive (and ours to signal).
fn process_alive(pid: u32) -> bool {
    Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Poll until `pid` is gone or the deadline passes; reports the elapsed time.
fn wait_until_gone(pid: u32, timeout: Duration) -> Option<Duration> {
    let started = Instant::now();
    while started.elapsed() < timeout {
        if !process_alive(pid) {
            return Some(started.elapsed());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    None
}

#[test]
fn drop_kills_and_reaps_within_two_seconds() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("herdr.sock");
    let server = FakeHerdrServer::spawn(FakeHerdrOptions::new(&socket)).unwrap();
    let pid = server.pid().expect("spawned server has a pid");
    assert!(process_alive(pid), "server should be running after spawn");

    drop(server);

    match wait_until_gone(pid, Duration::from_secs(2)) {
        Some(elapsed) => eprintln!("fake-herdr {pid} exited {elapsed:?} after drop"),
        None => panic!("fake-herdr {pid} still alive 2 s after dropping the handle"),
    }
    assert!(!socket.exists(), "drop must unlink the socket");
}

#[cfg(unix)]
#[test]
fn orphaned_server_exits_when_parent_is_killed() {
    use std::io::Read as _;

    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("herdr.sock");
    let pid_file = dir.path().join("server.pid");
    let bin = fake_herdr_bin();

    // A disposable wrapper spawns the server exactly as the old harness did
    // (stdin from /dev/null, so only pdeathsig/getppid can save us), records
    // the server pid, and waits. Killing *the wrapper* with SIGKILL mimics an
    // aborted gate: pre-fix the server was reparented to init and spun forever.
    let script = format!(
        "{bin} server --socket {socket} </dev/null >/dev/null 2>&1 &\n\
         echo $! > {pid_file}\n\
         wait\n",
        bin = bin.display(),
        socket = socket.display(),
        pid_file = pid_file.display(),
    );
    let mut wrapper = Command::new("sh")
        .arg("-c")
        .arg(script)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn wrapper shell");
    let wrapper_pid = wrapper.id();

    // Wait for the pidfile (and the socket, proving the server came up).
    let deadline = Instant::now() + Duration::from_secs(15);
    let server_pid = loop {
        if socket.exists() {
            let mut text = String::new();
            if std::fs::File::open(&pid_file)
                .and_then(|mut f| f.read_to_string(&mut text))
                .is_ok()
                && let Ok(pid) = text.trim().parse::<u32>()
            {
                break pid;
            }
        }
        assert!(Instant::now() < deadline, "server never came up");
        std::thread::sleep(Duration::from_millis(25));
    };
    assert!(process_alive(server_pid));

    // SIGKILL the parent out from under the server.
    let kill = Command::new("kill")
        .arg("-9")
        .arg(wrapper_pid.to_string())
        .status()
        .expect("kill wrapper");
    assert!(kill.success());
    let _ = wrapper.wait();

    match wait_until_gone(server_pid, Duration::from_secs(2)) {
        Some(elapsed) => eprintln!("orphaned fake-herdr exited {elapsed:?} after parent death"),
        None => panic!("fake-herdr {server_pid} survived its parent; abort cleanup would leak it"),
    }
}

#[cfg(target_os = "linux")]
#[test]
fn idle_server_uses_under_five_percent_cpu() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("herdr.sock");
    let server = FakeHerdrServer::spawn(FakeHerdrOptions::new(&socket)).unwrap();
    let pid = server.pid().expect("spawned server has a pid");

    // Userspace HZ is effectively always 100 on Linux, but ask the system so
    // a different scheduler tick rate cannot silently skew the assertion.
    let ticks_per_second = Command::new("getconf")
        .arg("CLK_TCK")
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .and_then(|text| text.trim().parse::<u64>().ok())
        .unwrap_or(100)
        .max(1);

    let stat_path = format!("/proc/{pid}/stat");
    let cpu_ticks = || -> u64 {
        let body = std::fs::read_to_string(&stat_path).expect("read /proc/<pid>/stat");
        // `comm` (field 2) can contain spaces and parentheses; take everything
        // after the final ')' and parse the space-separated remainder, where
        // utime is index 11 and stime index 12.
        let after_comm = body.rsplit_once(')').unwrap().1.trim_start();
        let fields: Vec<&str> = after_comm.split_whitespace().collect();
        fields[11].parse::<u64>().unwrap() + fields[12].parse::<u64>().unwrap()
    };

    // Let startup settle, then measure one idle second.
    std::thread::sleep(Duration::from_millis(200));
    let start_ticks = cpu_ticks();
    let started = Instant::now();
    std::thread::sleep(Duration::from_millis(1200));
    let elapsed = started.elapsed();
    let end_ticks = cpu_ticks();

    let cpu_seconds = (end_ticks - start_ticks) as f64 / ticks_per_second as f64;
    let percent = cpu_seconds / elapsed.as_secs_f64() * 100.0;
    eprintln!("idle fake-herdr used {percent:.2}% CPU over {elapsed:?}");

    // Keep the handle (and its kill-on-drop cleanup) alive until after the
    // measurement; assert before drop so a failure still cleans up.
    drop(server);
    assert!(
        percent < 5.0,
        "idle fake-herdr burned {percent:.1}% of a core (pre-fix: ~100%)"
    );
}
