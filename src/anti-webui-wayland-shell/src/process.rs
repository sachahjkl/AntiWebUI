//! Running commands, with what they print coming to the UI thread a line at a time (see "アプリ、
//! プロセス、システム" in `docs/design/37-wayland-shell.md`).
//!
//! ```no_run
//! use anti_webui_core::prelude::*;
//! use anti_webui_wayland_shell::Process;
//!
//! let ping = Process::new("ping").args(["-c", "3", "1.1.1.1"]).spawn();
//! let view = text(move || ping.lines().last().cloned().unwrap_or_default());
//! ```

use anti_webui_core::channel::ui_channel;
use anti_webui_core::reactive::{Signal, on_cleanup, signal};
use std::cell::RefCell;
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// How long `kill` waits after asking before it stops the process for good.
const GRACE: Duration = Duration::from_secs(2);

/// A command to run. See [`Process::spawn`].
#[derive(Clone, Debug)]
pub struct Process {
    program: OsString,
    args: Vec<OsString>,
    env: Vec<(OsString, OsString)>,
    dir: Option<PathBuf>,
    keep: usize,
    stdin: bool,
}

impl Process {
    pub fn new(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: Vec::new(),
            dir: None,
            keep: 1000,
            stdin: false,
        }
    }

    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn args<I: IntoIterator<Item = S>, S: Into<OsString>>(mut self, args: I) -> Self {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    pub fn current_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.dir = Some(dir.into());
        self
    }

    /// How many of the last lines of each output to keep; 1000 at first.
    pub fn keep(mut self, lines: usize) -> Self {
        self.keep = lines.max(1);
        self
    }

    /// Lets [`RunningProcess::write`] give it input; otherwise it reads nothing.
    pub fn stdin(mut self, on: bool) -> Self {
        self.stdin = on;
        self
    }

    /// Starts it, on this UI thread: what it prints comes here. It is killed when the owner this is
    /// called under goes. When it can't be started, the handle says so with an exit status of
    /// `None` and a line of the error in [`RunningProcess::error_lines`].
    pub fn spawn(self) -> RunningProcess {
        let p = RunningProcess {
            lines: signal(Vec::new()),
            errors: signal(Vec::new()),
            running: signal(true),
            status: signal(None),
            inner: signal(Rc::new(Inner::default())),
        };
        let keep = self.keep;
        let push = move |list: Signal<Vec<String>>, line: String| {
            list.update(|l| {
                l.push(line);
                if l.len() > keep {
                    l.drain(..l.len() - keep);
                }
            });
        };
        let sender = ui_channel(move |msg: Msg| match msg {
            Msg::Out(line) => {
                let listeners = p.inner.with_untracked(|i| i.on_line.borrow().clone());
                for f in listeners {
                    f(&line);
                }
                push(p.lines, line);
            }
            Msg::Err(line) => push(p.errors, line),
            Msg::Exit(code) => {
                p.inner.with_untracked(|i| i.stdin.borrow_mut().take());
                p.status.set(code);
                p.running.set(false);
                let listeners = p.inner.with_untracked(|i| i.on_exit.borrow().clone());
                for f in listeners {
                    f(code);
                }
            }
        });

        let mut command = Command::new(&self.program);
        command
            .args(&self.args)
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .stdin(if self.stdin {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Its own group, so that killing it takes what it started too.
            .process_group(0);
        if let Some(dir) = &self.dir {
            command.current_dir(dir);
        }
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(e) => {
                push(p.errors, format!("can't run {:?}: {e}", self.program));
                p.running.set(false);
                return p;
            }
        };
        let exited = Arc::new(AtomicBool::new(false));
        let pid = child.id() as i32;
        p.inner.with_untracked(|i| {
            *i.pid.borrow_mut() = Some((pid, exited.clone()));
            *i.stdin.borrow_mut() = child.stdin.take();
        });
        let readers = [
            child
                .stdout
                .take()
                .map(|o| read_lines(o, sender.clone(), Msg::Out)),
            child
                .stderr
                .take()
                .map(|e| read_lines(e, sender.clone(), Msg::Err)),
        ];
        let waited = exited.clone();
        let _ = std::thread::Builder::new()
            .name("anti-webui process".into())
            .spawn(move || {
                let code = wait(&mut child);
                let exited = waited;
                exited.store(true, Ordering::Release);
                // What it printed comes before it is said to have ended.
                for reader in readers.into_iter().flatten() {
                    let _ = reader.join();
                }
                sender.send(Msg::Exit(code));
            });
        // Not through `p`: its signals may be gone by then.
        on_cleanup(move || terminate(pid, exited));
        p
    }
}

/// The exit code, or minus the signal that ended it.
fn wait(child: &mut Child) -> Option<i32> {
    let status = child.wait().ok()?;
    status.code().or_else(|| status.signal().map(|s| -s))
}

fn read_lines(
    from: impl Read + Send + 'static,
    sender: anti_webui_core::channel::UiSender<Msg>,
    wrap: fn(String) -> Msg,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(from);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
            if buf.last() == Some(&b'\n') {
                buf.pop();
            }
            if buf.last() == Some(&b'\r') {
                buf.pop();
            }
            sender.send(wrap(String::from_utf8_lossy(&buf).into_owned()));
        }
    })
}

enum Msg {
    Out(String),
    Err(String),
    Exit(Option<i32>),
}

type LineListener = Rc<dyn Fn(&str)>;
type ExitListener = Rc<dyn Fn(Option<i32>)>;

#[derive(Default)]
struct Inner {
    /// Its process id, and whether it has been waited for, after which the id may be another's.
    pid: RefCell<Option<(i32, Arc<AtomicBool>)>>,
    stdin: RefCell<Option<ChildStdin>>,
    on_line: RefCell<Vec<LineListener>>,
    on_exit: RefCell<Vec<ExitListener>>,
}

/// A command that was started. It can be copied into closures like a signal, on the thread it was
/// started on.
#[derive(Clone, Copy)]
pub struct RunningProcess {
    lines: Signal<Vec<String>>,
    errors: Signal<Vec<String>>,
    running: Signal<bool>,
    status: Signal<Option<i32>>,
    inner: Signal<Rc<Inner>>,
}

impl std::fmt::Debug for RunningProcess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunningProcess")
            .field("running", &self.running.get_untracked())
            .field("status", &self.status.get_untracked())
            .finish_non_exhaustive()
    }
}

impl RunningProcess {
    /// The last lines it printed, as many as [`Process::keep`] says.
    pub fn lines(&self) -> Vec<String> {
        self.lines.get()
    }

    /// The last lines it printed as errors.
    pub fn error_lines(&self) -> Vec<String> {
        self.errors.get()
    }

    pub fn running(&self) -> bool {
        self.running.get()
    }

    /// Once it has ended, its exit code, or minus the signal that ended it. `None` while it runs
    /// or when it couldn't be started.
    pub fn exit_status(&self) -> Option<i32> {
        self.status.get()
    }

    /// Calls `f` with each line it prints from now on.
    pub fn on_line(&self, f: impl Fn(&str) + 'static) {
        self.inner
            .with_untracked(|i| i.on_line.borrow_mut().push(Rc::new(f)));
    }

    /// Calls `f` when it ends, at once if it has.
    pub fn on_exit(&self, f: impl Fn(Option<i32>) + 'static) {
        if !self.running.get_untracked() {
            f(self.status.get_untracked());
            return;
        }
        self.inner
            .with_untracked(|i| i.on_exit.borrow_mut().push(Rc::new(f)));
    }

    /// Gives it input, if it was started with [`Process::stdin`]. Returns whether that went.
    pub fn write(&self, text: &str) -> bool {
        self.inner.with_untracked(|i| {
            let mut stdin = i.stdin.borrow_mut();
            let Some(pipe) = stdin.as_mut() else {
                return false;
            };
            let ok = pipe
                .write_all(text.as_bytes())
                .and_then(|()| pipe.flush())
                .is_ok();
            if !ok {
                *stdin = None;
            }
            ok
        })
    }

    /// Ends its input, which tells a program reading it that there is no more.
    pub fn close_stdin(&self) {
        self.inner.with_untracked(|i| i.stdin.borrow_mut().take());
    }

    /// Asks it and what it started to stop (`SIGTERM`), and stops them for good (`SIGKILL`) if they
    /// are still there after two seconds.
    pub fn kill(&self) {
        if !self.inner.is_alive() {
            return;
        }
        if let Some((pid, exited)) = self.inner.with_untracked(|i| i.pid.borrow().clone()) {
            terminate(pid, exited);
        }
    }
}

/// `SIGTERM` to the process's group, then `SIGKILL` if it is still there after [`GRACE`].
fn terminate(pid: i32, exited: Arc<AtomicBool>) {
    if exited.load(Ordering::Acquire) {
        return;
    }
    // SAFETY: a plain call; the group is the process's own, which hasn't been waited for.
    unsafe { libc::kill(-pid, libc::SIGTERM) };
    let _ = std::thread::Builder::new()
        .name("anti-webui kill".into())
        .spawn(move || {
            let end = std::time::Instant::now() + GRACE;
            while std::time::Instant::now() < end {
                if exited.load(Ordering::Acquire) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            // SAFETY: as above.
            unsafe { libc::kill(-pid, libc::SIGKILL) };
        });
}
