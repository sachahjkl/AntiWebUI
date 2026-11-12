//! Running commands: their output, their end, input, killing them. Run with `--features process`.

#![cfg(target_os = "linux")]
#![cfg(feature = "process")]

use anti_webui_core::reactive::create_root;
use anti_webui_wayland_shell::Process;
use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

fn until(what: &str, mut done: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(5);
    while !done() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        anti_webui_core::channel::pump();
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Whether a process is there, and not only waiting to be reaped.
fn alive(pid: i32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .map(|s| {
            !s.split(") ")
                .nth(1)
                .is_some_and(|rest| rest.starts_with('Z'))
        })
        .unwrap_or(false)
}

/// The pid a `sh -c 'echo $$; …'` printed first.
fn first_pid(lines: &[String]) -> Option<i32> {
    lines.first()?.trim().parse().ok()
}

#[test]
fn commands_print_end_take_input_and_are_killed() {
    let ((), _owner) = create_root(|| {
        // Output, errors, the exit code, listeners.
        let heard = Rc::new(RefCell::new(Vec::new()));
        let exited = Rc::new(RefCell::new(None));
        let p = Process::new("sh")
            .args(["-c", "echo a; echo b >&2; printf 'c\\r\\nd'; exit 3"])
            .spawn();
        let h = heard.clone();
        p.on_line(move |l| h.borrow_mut().push(l.to_owned()));
        let e = exited.clone();
        p.on_exit(move |code| *e.borrow_mut() = Some(code));
        until("the end", || !p.running());
        assert_eq!(p.lines(), ["a", "c", "d"]);
        assert_eq!(p.error_lines(), ["b"]);
        assert_eq!(p.exit_status(), Some(3));
        assert_eq!(*heard.borrow(), ["a", "c", "d"]);
        assert_eq!(*exited.borrow(), Some(Some(3)));

        // Only the last lines are kept.
        let p = Process::new("seq").arg("10").keep(3).spawn();
        until("seq", || !p.running());
        assert_eq!(p.lines(), ["8", "9", "10"]);

        // Input.
        let p = Process::new("cat").stdin(true).spawn();
        assert!(p.write("hello\n"));
        until("the echo", || p.lines() == ["hello"]);
        p.close_stdin();
        until("cat to end", || !p.running());
        assert_eq!(p.exit_status(), Some(0));

        // What can't be run.
        let p = Process::new("/nonexistent/anti-webui").spawn();
        assert!(!p.running());
        assert_eq!(p.exit_status(), None);
        assert_eq!(p.error_lines().len(), 1);

        // kill() takes the group: the shell and the sleep it started.
        let p = Process::new("sh")
            .args(["-c", "echo $$; sleep 30 & echo $!; wait"])
            .spawn();
        until("the pids", || p.lines().len() == 2);
        let (sh, sleep) = (
            first_pid(&p.lines()).unwrap(),
            p.lines()[1].trim().parse::<i32>().unwrap(),
        );
        assert!(alive(sh) && alive(sleep));
        p.kill();
        until("the kill", || !p.running());
        assert_eq!(p.exit_status(), Some(-libc::SIGTERM));
        until("the sleep to go", || !alive(sleep));
    });

    // The owner going kills what was spawned under it.
    let (pid, owner) = create_root(|| {
        let p = Process::new("sh")
            .args(["-c", "echo $$; exec sleep 30"])
            .spawn();
        until("the pid", || !p.lines().is_empty());
        first_pid(&p.lines()).unwrap()
    });
    assert!(alive(pid));
    owner.dispose();
    until("the process to go", || !alive(pid));
}
