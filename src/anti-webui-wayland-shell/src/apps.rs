//! The applications installed: the XDG `.desktop` files, read on a thread nobody sees and followed
//! as they change, searching them, their icons from the icon theme, and launching them (see
//! "アプリ、プロセス、システム" in `docs/design/37-wayland-shell.md`).

use crate::ServiceError;
use crate::conn::{State as Conn, link};
use anti_webui_core::shared::{SharedSignal, shared_signal};
use anti_webui_core::task::{Task, task};
use std::collections::{HashMap, HashSet};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::xdg::activation::v1::client::{
    xdg_activation_token_v1 as token, xdg_activation_v1 as activation,
};

/// An application, as its `.desktop` file said when read.
#[derive(Clone, Debug, PartialEq)]
pub struct Application {
    /// The desktop file id, like `org.mozilla.firefox.desktop`.
    pub id: String,
    /// In the language of the user, if the file has it.
    pub name: String,
    /// What it is, like "Web Browser".
    pub generic_name: Option<String>,
    pub comment: Option<String>,
    /// The `Icon=` value: a name for the icon theme, or a path. See [`Application::icon_path`].
    pub icon: Option<String>,
    pub keywords: Vec<String>,
    pub categories: Vec<String>,
    /// It runs in a terminal.
    pub terminal: bool,
    /// What else it can be started to do, like a new private window.
    pub actions: Vec<AppAction>,
    exec: String,
    context: Arc<Context>,
}

/// Something else an application can be started to do.
#[derive(Clone, Debug, PartialEq)]
pub struct AppAction {
    pub id: String,
    pub name: String,
    pub icon: Option<String>,
    exec: String,
    context: Arc<Context>,
}

/// What launching needs besides the command.
#[derive(Debug, PartialEq)]
struct Context {
    id: String,
    name: String,
    icon: Option<String>,
    /// The `.desktop` file.
    path: PathBuf,
    /// `Path=`: where to run it.
    dir: Option<PathBuf>,
    terminal: bool,
}

impl Application {
    /// A file for its icon at about `size` pixels, from the icon theme.
    pub fn icon_path(&self, size: u32) -> Option<PathBuf> {
        icon_path(self.icon.as_deref()?, size)
    }

    /// Starts it, apart from this program: it goes on when this ends. It gets an activation token,
    /// so that the compositor lets it come to the front.
    pub fn launch(&self) -> Task<Result<(), ServiceError>> {
        self.launch_in(Vec::new())
    }

    /// [`launch`](Self::launch), its command line after `prefix`: a program that runs the
    /// application in its own place, such as `systemd-run --user --scope … --`.
    pub fn launch_in(&self, prefix: Vec<String>) -> Task<Result<(), ServiceError>> {
        let (exec, context) = (self.exec.clone(), self.context.clone());
        task(move || launch(&exec, &context, &prefix))
    }
}

impl AppAction {
    pub fn icon_path(&self, size: u32) -> Option<PathBuf> {
        icon_path(self.icon.as_deref().or(self.context.icon.as_deref())?, size)
    }

    pub fn launch(&self) -> Task<Result<(), ServiceError>> {
        self.launch_in(Vec::new())
    }

    /// [`Application::launch_in`] for the action.
    pub fn launch_in(&self, prefix: Vec<String>) -> Task<Result<(), ServiceError>> {
        let (exec, context) = (self.exec.clone(), self.context.clone());
        task(move || launch(&exec, &context, &prefix))
    }
}

/// The applications. Cheap to copy; reading tracks, on any thread.
#[derive(Clone, Copy, Debug)]
pub struct Apps(&'static Shared);

#[derive(Debug)]
struct Shared {
    list: SharedSignal<Vec<Application>>,
    loaded: SharedSignal<bool>,
}

static SHARED: OnceLock<Shared> = OnceLock::new();

/// The applications. They are read the first time, and followed from then on.
pub fn apps() -> Apps {
    Apps(SHARED.get_or_init(|| {
        let _ = std::thread::Builder::new()
            .name("anti-webui apps".into())
            .spawn(follow);
        Shared {
            list: shared_signal(Vec::new()),
            loaded: shared_signal(false),
        }
    }))
}

impl Apps {
    /// Whether they have been read once; until then there are none.
    pub fn loaded(&self) -> bool {
        self.0.loaded.get()
    }

    /// Those to show, by name.
    pub fn all(&self) -> Vec<Application> {
        self.0.list.get()
    }

    pub fn get(&self, id: &str) -> Option<Application> {
        self.0.list.with(|l| l.iter().find(|a| a.id == id).cloned())
    }

    /// Those that match, best first: the start of the name, of a word of it, its initials, the
    /// start of a word of the generic name, then anywhere in the name, the generic name, the
    /// keywords and the command. All of them for nothing.
    pub fn search(&self, query: &str) -> Vec<Application> {
        let query = query.trim().to_lowercase();
        let all = self.all();
        if query.is_empty() {
            return all;
        }
        let mut found: Vec<(u8, Application)> = all
            .into_iter()
            .filter_map(|a| Some((rank(&a, &query)?, a)))
            .collect();
        // Stable, so that those of a rank stay by name.
        found.sort_by_key(|(r, _)| *r);
        found.into_iter().map(|(_, a)| a).collect()
    }
}

/// How well an application matches a query in lower case; lower is better.
fn rank(app: &Application, query: &str) -> Option<u8> {
    let words = |s: &str| -> Vec<String> {
        s.split(|c: char| c.is_whitespace() || c == '-' || c == '_' || c == ',')
            .filter(|w| !w.is_empty())
            .map(str::to_owned)
            .collect()
    };
    let name = app.name.to_lowercase();
    let name_words = words(&name);
    let initials: String = name_words.iter().filter_map(|w| w.chars().next()).collect();
    let generic = app
        .generic_name
        .as_deref()
        .unwrap_or_default()
        .to_lowercase();
    if name.starts_with(query) {
        Some(0)
    } else if name_words.iter().any(|w| w.starts_with(query)) {
        Some(1)
    } else if query.chars().count() >= 2 && initials.starts_with(query) {
        Some(2)
    } else if words(&generic).iter().any(|w| w.starts_with(query)) {
        Some(3)
    } else if name.contains(query) {
        Some(4)
    } else if generic.contains(query) {
        Some(5)
    } else if app
        .keywords
        .iter()
        .any(|k| k.to_lowercase().contains(query))
    {
        Some(6)
    } else if app.exec.to_lowercase().contains(query) || app.id.to_lowercase().contains(query) {
        Some(7)
    } else {
        None
    }
}

// The XDG base directories.

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

fn env_dir(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

fn data_home() -> Option<PathBuf> {
    env_dir("XDG_DATA_HOME").or_else(|| Some(home()?.join(".local/share")))
}

fn config_home() -> Option<PathBuf> {
    env_dir("XDG_CONFIG_HOME").or_else(|| Some(home()?.join(".config")))
}

fn data_dirs() -> Vec<PathBuf> {
    let dirs = std::env::var("XDG_DATA_DIRS").unwrap_or_default();
    let dirs: Vec<PathBuf> = dirs
        .split(':')
        .filter(|d| d.starts_with('/'))
        .map(PathBuf::from)
        .collect();
    if dirs.is_empty() {
        vec!["/usr/local/share".into(), "/usr/share".into()]
    } else {
        dirs
    }
}

/// Where data is looked for, most important first.
fn data_search() -> Vec<PathBuf> {
    data_home().into_iter().chain(data_dirs()).collect()
}

// Desktop entries.

/// The groups of a key file, each with its keys in order.
type Groups = Vec<(String, Vec<(String, String)>)>;

fn parse_key_file(text: &str) -> Groups {
    let mut groups: Groups = Vec::new();
    for line in text.lines() {
        let line = line.trim_start();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            groups.push((name.to_owned(), Vec::new()));
        } else if let (Some((key, value)), Some((_, keys))) =
            (line.split_once('='), groups.last_mut())
        {
            keys.push((key.trim().to_owned(), value.trim().to_owned()));
        }
    }
    groups
}

/// A group of a key file, read with the user's languages.
struct Group<'a> {
    keys: &'a [(String, String)],
    locales: &'a [String],
}

impl Group<'_> {
    fn raw(&self, key: &str) -> Option<&str> {
        self.keys
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// A string, in the best language the file has for the user.
    fn string(&self, key: &str) -> Option<String> {
        self.locales
            .iter()
            .find_map(|l| self.raw(&format!("{key}[{l}]")))
            .or_else(|| self.raw(key))
            .map(unescape)
            .filter(|s| !s.is_empty())
    }

    fn bool(&self, key: &str) -> bool {
        self.raw(key) == Some("true")
    }

    fn list(&self, key: &str) -> Vec<String> {
        let raw = self
            .locales
            .iter()
            .find_map(|l| self.raw(&format!("{key}[{l}]")))
            .or_else(|| self.raw(key));
        raw.map(split_list).unwrap_or_default()
    }
}

/// What a string value escapes: `\s`, `\n`, `\t`, `\r` and `\\`.
fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('s') => out.push(' '),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// A list value: separated by `;`, which `\;` escapes.
fn split_list(value: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut item = String::new();
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some(';') => item.push(';'),
                Some(other) => {
                    item.push('\\');
                    item.push(other);
                }
                None => {}
            },
            ';' => items.push(std::mem::take(&mut item)),
            c => item.push(c),
        }
    }
    items.push(item);
    items
        .into_iter()
        .map(|i| unescape(&i))
        .filter(|i| !i.is_empty())
        .collect()
}

/// The user's languages in the order the spec matches them: `lang_COUNTRY@MODIFIER`,
/// `lang_COUNTRY`, `lang@MODIFIER`, `lang`.
fn locales() -> Vec<String> {
    let locale = ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
        .unwrap_or_default();
    locales_of(&locale)
}

fn locales_of(locale: &str) -> Vec<String> {
    let (rest, modifier) = match locale.split_once('@') {
        Some((r, m)) => (r, Some(m)),
        None => (locale, None),
    };
    let rest = rest.split('.').next().unwrap_or_default();
    let (lang, country) = match rest.split_once('_') {
        Some((l, c)) => (l, Some(c)),
        None => (rest, None),
    };
    if lang.is_empty() || lang == "C" || lang == "POSIX" {
        return Vec::new();
    }
    let mut out = Vec::new();
    if let (Some(c), Some(m)) = (country, modifier) {
        out.push(format!("{lang}_{c}@{m}"));
    }
    if let Some(c) = country {
        out.push(format!("{lang}_{c}"));
    }
    if let Some(m) = modifier {
        out.push(format!("{lang}@{m}"));
    }
    out.push(lang.to_owned());
    out
}

/// The desktops the user is in, from `$XDG_CURRENT_DESKTOP`.
fn current_desktops() -> Vec<String> {
    std::env::var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .split(':')
        .filter(|d| !d.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Whether a program is on `PATH`, or is a path to one.
fn on_path(program: &str) -> bool {
    if program.contains('/') {
        return Path::new(program).is_file();
    }
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|d| d.join(program).is_file()))
}

/// An application to show from a desktop file, if it is one.
fn read_app(
    id: &str,
    path: &Path,
    text: &str,
    desktops: &[String],
    locales: &[String],
) -> Option<Application> {
    let groups = parse_key_file(text);
    let (_, keys) = groups.iter().find(|(g, _)| g == "Desktop Entry")?;
    let g = Group { keys, locales };
    if g.raw("Type") != Some("Application") || g.bool("NoDisplay") || g.bool("Hidden") {
        return None;
    }
    let only = g.list("OnlyShowIn");
    if !only.is_empty() && !only.iter().any(|d| desktops.contains(d)) {
        return None;
    }
    if g.list("NotShowIn").iter().any(|d| desktops.contains(d)) {
        return None;
    }
    if let Some(try_exec) = g.string("TryExec")
        && !on_path(&try_exec)
    {
        return None;
    }
    let exec = g.raw("Exec").map(unescape)?;
    let name = g.string("Name")?;
    let icon = g.string("Icon");
    let context = Arc::new(Context {
        id: id.to_owned(),
        name: name.clone(),
        icon: icon.clone(),
        path: path.to_owned(),
        dir: g.string("Path").map(PathBuf::from),
        terminal: g.bool("Terminal"),
    });
    let actions = g
        .list("Actions")
        .into_iter()
        .filter_map(|action| {
            let (_, keys) = groups
                .iter()
                .find(|(n, _)| *n == format!("Desktop Action {action}"))?;
            let a = Group { keys, locales };
            Some(AppAction {
                name: a.string("Name")?,
                icon: a.string("Icon"),
                exec: a.raw("Exec").map(unescape)?,
                context: context.clone(),
                id: action,
            })
        })
        .collect();
    Some(Application {
        id: id.to_owned(),
        name,
        generic_name: g.string("GenericName"),
        comment: g.string("Comment"),
        icon,
        keywords: g.list("Keywords"),
        categories: g.list("Categories"),
        terminal: g.bool("Terminal"),
        actions,
        exec,
        context,
    })
}

/// Every application to show, by name, and the directories they were looked for in.
fn scan() -> (Vec<Application>, Vec<PathBuf>) {
    let (desktops, locales) = (current_desktops(), locales());
    let mut seen = HashSet::new();
    let mut apps = Vec::new();
    let mut dirs = Vec::new();
    for base in data_search() {
        let root = base.join("applications");
        let mut stack = vec![root.clone()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            dirs.push(dir.clone());
            let mut files = Vec::new();
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|e| e == "desktop") {
                    files.push(path);
                }
            }
            files.sort();
            for path in files {
                let Ok(rel) = path.strip_prefix(&root) else {
                    continue;
                };
                let id = rel.to_string_lossy().replace('/', "-");
                // An earlier directory's file of the same id hides this one, shown or not.
                if !seen.insert(id.clone()) {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                if let Some(app) = read_app(&id, &path, &text, &desktops, &locales) {
                    apps.push(app);
                }
            }
        }
    }
    apps.sort_by_cached_key(|a| (a.name.to_lowercase(), a.id.clone()));
    (apps, dirs)
}

/// Reads them, then reads again whenever a directory of them changes.
fn follow() {
    let shared = loop {
        if let Some(s) = SHARED.get() {
            break s;
        }
        std::thread::sleep(Duration::from_millis(1));
    };
    loop {
        let (list, dirs) = scan();
        if shared.list.with_untracked(|l| *l != list) {
            shared.list.set(list);
        }
        if !shared.loaded.get_untracked() {
            shared.loaded.set(true);
        }
        // The data home too, to see `applications` made there.
        let watched: Vec<PathBuf> = dirs.into_iter().chain(data_home()).collect();
        if !wait_for_change(&watched) {
            return;
        }
    }
}

/// Waits until something changes in `dirs`, and a moment more for what comes with it. False when
/// changes can't be followed.
fn wait_for_change(dirs: &[PathBuf]) -> bool {
    // SAFETY: plain calls on a descriptor this owns and closes.
    unsafe {
        let fd = libc::inotify_init1(libc::IN_CLOEXEC);
        if fd < 0 {
            return false;
        }
        let mask = libc::IN_CREATE
            | libc::IN_DELETE
            | libc::IN_CLOSE_WRITE
            | libc::IN_MOVED_FROM
            | libc::IN_MOVED_TO
            | libc::IN_DELETE_SELF
            | libc::IN_ATTRIB;
        for dir in dirs {
            if let Ok(path) = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()) {
                libc::inotify_add_watch(fd, path.as_ptr(), mask);
            }
        }
        let mut buf = [0u8; 4096];
        let mut poll = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let mut ok = libc::poll(&mut poll, 1, -1) > 0;
        // A package manager writes many files at once.
        while ok && libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) > 0 {
            if libc::poll(&mut poll, 1, 300) <= 0 {
                break;
            }
        }
        if !ok {
            ok = std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted;
        }
        libc::close(fd);
        ok
    }
}

// Launching.

/// Splits an `Exec=` value into arguments, as the spec quotes them.
fn split_exec(exec: &str) -> Option<Vec<String>> {
    let mut args = Vec::new();
    let mut arg = String::new();
    let mut in_arg = false;
    let mut chars = exec.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                in_arg = true;
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' => match chars.next()? {
                            c @ ('"' | '`' | '$' | '\\') => arg.push(c),
                            c => {
                                arg.push('\\');
                                arg.push(c);
                            }
                        },
                        c => arg.push(c),
                    }
                }
            }
            ' ' | '\t' => {
                if in_arg {
                    args.push(std::mem::take(&mut arg));
                    in_arg = false;
                }
            }
            c => {
                in_arg = true;
                arg.push(c);
            }
        }
    }
    if in_arg {
        args.push(arg);
    }
    Some(args)
}

/// The arguments with the field codes put in or taken out: no files or URLs are given.
fn expand(args: Vec<String>, context: &Context) -> Vec<String> {
    let mut out = Vec::new();
    for arg in args {
        match arg.as_str() {
            "%f" | "%F" | "%u" | "%U" | "%d" | "%D" | "%n" | "%N" | "%v" | "%m" => {}
            "%i" => {
                if let Some(icon) = &context.icon {
                    out.push("--icon".into());
                    out.push(icon.clone());
                }
            }
            _ => {
                let mut s = String::with_capacity(arg.len());
                let mut chars = arg.chars();
                while let Some(c) = chars.next() {
                    if c != '%' {
                        s.push(c);
                        continue;
                    }
                    match chars.next() {
                        Some('%') => s.push('%'),
                        Some('c') => s.push_str(&context.name),
                        Some('k') => s.push_str(&context.path.to_string_lossy()),
                        _ => {}
                    }
                }
                out.push(s);
            }
        }
    }
    out
}

/// How to run a command in a terminal: `$TERMINAL`, or the first of the common ones there is.
fn in_terminal(argv: Vec<String>) -> Option<Vec<String>> {
    let terminal = std::env::var("TERMINAL")
        .ok()
        .filter(|t| !t.is_empty() && on_path(t))
        .or_else(|| {
            [
                "foot",
                "kitty",
                "alacritty",
                "wezterm",
                "gnome-terminal",
                "konsole",
                "xterm",
            ]
            .into_iter()
            .find(|t| on_path(t))
            .map(str::to_owned)
        })?;
    let name = Path::new(&terminal)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    // How each takes the command after it; `-e` for those not known.
    let before: &[&str] = match name.as_str() {
        "foot" | "kitty" => &[],
        "wezterm" => &["start", "--"],
        "gnome-terminal" => &["--"],
        _ => &["-e"],
    };
    Some(
        std::iter::once(terminal)
            .chain(before.iter().map(|s| (*s).to_owned()))
            .chain(argv)
            .collect(),
    )
}

fn launch(exec: &str, context: &Context, prefix: &[String]) -> Result<(), ServiceError> {
    let failed = |why: String| ServiceError::Failed(why);
    let mut argv = expand(
        split_exec(exec).ok_or_else(|| failed(format!("can't read Exec={exec}")))?,
        context,
    );
    if argv.is_empty() {
        return Err(failed("Exec is empty".into()));
    }
    if context.terminal {
        argv = in_terminal(argv).ok_or_else(|| failed("no terminal to run it in".into()))?;
    }
    if !prefix.is_empty() {
        argv = prefix.iter().cloned().chain(argv).collect();
    }
    let app_id = context.id.strip_suffix(".desktop").unwrap_or(&context.id);
    let token = activation_token(app_id);
    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env_remove("XDG_ACTIVATION_TOKEN")
        .env_remove("DESKTOP_STARTUP_ID");
    if let Some(dir) = context.dir.as_ref().filter(|d| d.is_dir()) {
        command.current_dir(dir);
    }
    if let Some(token) = token {
        command
            .env("XDG_ACTIVATION_TOKEN", &token)
            .env("DESKTOP_STARTUP_ID", &token);
    }
    // SAFETY: only async-signal-safe calls between fork and exec.
    unsafe {
        command.pre_exec(|| {
            // Forks once more and lets the middle one go, so that the application belongs to
            // init rather than to this program, and in a session of its own.
            match libc::fork() {
                -1 => return Err(std::io::Error::last_os_error()),
                0 => {}
                _ => libc::_exit(0),
            }
            libc::setsid();
            Ok(())
        });
    }
    let mut middle = command
        .spawn()
        .map_err(|e| failed(format!("can't run {}: {e}", argv[0])))?;
    let _ = middle.wait();
    Ok(())
}

/// What the token being asked for gets.
type Waiting = Arc<(Mutex<Option<String>>, Condvar)>;

static ACTIVATION: OnceLock<Option<activation::XdgActivationV1>> = OnceLock::new();

/// A token for the launched application to come to the front with, if the compositor gives one
/// within half a second.
fn activation_token(app_id: &str) -> Option<String> {
    let link = link()?;
    let manager = ACTIVATION.get_or_init(|| link.bind(1, ())).as_ref()?;
    let waiting: Waiting = Arc::default();
    let t = manager.get_activation_token(&link.qh, waiting.clone());
    t.set_app_id(app_id.to_owned());
    t.commit();
    link.flush();
    let (lock, cvar) = &*waiting;
    let guard = lock.lock().ok()?;
    let (guard, _) = cvar
        .wait_timeout_while(guard, Duration::from_millis(500), |t| t.is_none())
        .ok()?;
    let token = guard.clone();
    drop(guard);
    t.destroy();
    link.flush();
    token
}

impl Dispatch<token::XdgActivationTokenV1, Waiting> for Conn {
    fn event(
        _: &mut Self,
        _: &token::XdgActivationTokenV1,
        event: token::Event,
        waiting: &Waiting,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let token::Event::Done { token } = event {
            let (lock, cvar) = &**waiting;
            if let Ok(mut t) = lock.lock() {
                *t = Some(token);
            }
            cvar.notify_all();
        }
    }
}

wayland_client::delegate_noop!(Conn: activation::XdgActivationV1);

// Icons.

/// How a directory of a theme says which sizes it is for.
#[derive(Clone, Copy, Debug, PartialEq)]
enum DirKind {
    Fixed,
    Scalable { min: u32, max: u32 },
    Threshold(u32),
}

#[derive(Debug)]
struct ThemeDir {
    path: String,
    size: u32,
    scale: u32,
    kind: DirKind,
}

impl ThemeDir {
    fn matches(&self, size: u32) -> bool {
        self.scale == 1
            && match self.kind {
                DirKind::Fixed => self.size == size,
                DirKind::Scalable { min, max } => (min..=max).contains(&size),
                DirKind::Threshold(t) => {
                    (self.size.saturating_sub(t)..=self.size + t).contains(&size)
                }
            }
    }

    fn distance(&self, size: u32) -> u32 {
        let (s, size) = (self.scale, size);
        let (low, high) = match self.kind {
            DirKind::Fixed => (self.size, self.size),
            DirKind::Scalable { min, max } => (min, max),
            DirKind::Threshold(t) => (self.size.saturating_sub(t), self.size + t),
        };
        if size < low * s {
            low * s - size
        } else {
            size.saturating_sub(high * s)
        }
    }

    /// The order to try extensions in.
    fn extensions(&self) -> [&'static str; 3] {
        match self.kind {
            DirKind::Scalable { .. } => ["svg", "png", "xpm"],
            _ => ["png", "svg", "xpm"],
        }
    }
}

#[derive(Debug)]
struct Theme {
    /// The directories that have it, under each place icons are.
    roots: Vec<PathBuf>,
    dirs: Vec<ThemeDir>,
    inherits: Vec<String>,
}

/// Where icon themes are: `~/.icons`, then `icons` in each data directory.
fn icon_bases() -> Vec<PathBuf> {
    home()
        .map(|h| h.join(".icons"))
        .into_iter()
        .chain(data_search().into_iter().map(|d| d.join("icons")))
        .collect()
}

fn parse_theme(index: &str, roots: Vec<PathBuf>) -> Theme {
    let groups = parse_key_file(index);
    let empty = Vec::new();
    let main = groups
        .iter()
        .find(|(g, _)| g == "Icon Theme")
        .map(|(_, k)| k)
        .unwrap_or(&empty);
    let g = Group {
        keys: main,
        locales: &[],
    };
    // Unlike desktop entries, these lists are separated by commas.
    let commas = |key: &str| -> Vec<String> {
        g.raw(key)
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect()
    };
    let mut names = commas("Directories");
    names.extend(commas("ScaledDirectories"));
    let dirs = names
        .into_iter()
        .filter_map(|path| {
            let (_, keys) = groups.iter().find(|(n, _)| *n == path)?;
            let d = Group { keys, locales: &[] };
            let num = |k: &str| d.raw(k).and_then(|v| v.parse::<u32>().ok());
            let size = num("Size")?;
            let kind = match d.raw("Type").unwrap_or("Threshold") {
                "Fixed" => DirKind::Fixed,
                "Scalable" => DirKind::Scalable {
                    min: num("MinSize").unwrap_or(size),
                    max: num("MaxSize").unwrap_or(size),
                },
                _ => DirKind::Threshold(num("Threshold").unwrap_or(2)),
            };
            Some(ThemeDir {
                path,
                size,
                scale: num("Scale").unwrap_or(1),
                kind,
            })
        })
        .collect();
    Theme {
        roots,
        dirs,
        inherits: commas("Inherits"),
    }
}

static THEMES: Mutex<Option<HashMap<String, Option<Arc<Theme>>>>> = Mutex::new(None);
/// Icons looked for by name and size, found or not.
type Found = HashMap<(String, u32), Option<PathBuf>>;

static FOUND: Mutex<Option<Found>> = Mutex::new(None);

fn theme(name: &str) -> Option<Arc<Theme>> {
    let mut themes = THEMES.lock().ok()?;
    let themes = themes.get_or_insert_with(HashMap::new);
    themes
        .entry(name.to_owned())
        .or_insert_with(|| {
            let roots: Vec<PathBuf> = icon_bases()
                .into_iter()
                .map(|b| b.join(name))
                .filter(|r| r.is_dir())
                .collect();
            let index = roots
                .iter()
                .find_map(|r| std::fs::read_to_string(r.join("index.theme")).ok())?;
            Some(Arc::new(parse_theme(&index, roots)))
        })
        .clone()
}

/// The icon theme the user chose: GTK's setting, then KDE's, else `hicolor`.
fn theme_name() -> String {
    let config = config_home();
    let read = |rel: &str, group: &str, key: &str| {
        let text = std::fs::read_to_string(config.as_ref()?.join(rel)).ok()?;
        let groups = parse_key_file(&text);
        let (_, keys) = groups.iter().find(|(g, _)| g == group)?;
        Group { keys, locales: &[] }.string(key)
    };
    read("gtk-3.0/settings.ini", "Settings", "gtk-icon-theme-name")
        .or_else(|| read("gtk-4.0/settings.ini", "Settings", "gtk-icon-theme-name"))
        .or_else(|| read("kdeglobals", "Icons", "Theme"))
        .unwrap_or_else(|| "hicolor".into())
}

/// The themes to look in, in order: the user's, those it inherits from, then `hicolor`.
fn theme_chain() -> Vec<Arc<Theme>> {
    let mut names = vec![theme_name()];
    let mut chain = Vec::new();
    let mut seen = HashSet::new();
    while let Some(name) = (!names.is_empty()).then(|| names.remove(0)) {
        if !seen.insert(name.clone()) {
            continue;
        }
        if let Some(t) = theme(&name) {
            // Depth first: what this one inherits comes before what is after it.
            for (i, parent) in t.inherits.iter().enumerate() {
                names.insert(i, parent.clone());
            }
            chain.push(t);
        }
    }
    if !seen.contains("hicolor")
        && let Some(t) = theme("hicolor")
    {
        chain.push(t);
    }
    chain
}

fn look_in(theme: &Theme, name: &str, size: u32) -> Option<PathBuf> {
    let file = |dir: &ThemeDir| {
        theme.roots.iter().find_map(|root| {
            dir.extensions().iter().find_map(|ext| {
                let path = root.join(&dir.path).join(format!("{name}.{ext}"));
                path.is_file().then_some(path)
            })
        })
    };
    if let Some(found) = theme.dirs.iter().filter(|d| d.matches(size)).find_map(file) {
        return Some(found);
    }
    let mut by_distance: Vec<&ThemeDir> = theme.dirs.iter().collect();
    by_distance.sort_by_key(|d| d.distance(size));
    by_distance.into_iter().find_map(file)
}

/// A file for an icon at about `size` pixels: a path as it is, else from the icon theme, the
/// themes it inherits from and `hicolor`, else from `/usr/share/pixmaps`.
pub fn icon_path(name: &str, size: u32) -> Option<PathBuf> {
    if name.starts_with('/') {
        let path = PathBuf::from(name);
        return path.is_file().then_some(path);
    }
    // Some files give the name with its extension.
    let name = ["png", "svg", "xpm"]
        .iter()
        .find_map(|ext| name.strip_suffix(&format!(".{ext}")))
        .unwrap_or(name);
    let key = (name.to_owned(), size);
    if let Ok(mut found) = FOUND.lock()
        && let Some(hit) = found.get_or_insert_with(HashMap::new).get(&key)
    {
        return hit.clone();
    }
    let found = theme_chain()
        .iter()
        .find_map(|t| look_in(t, name, size))
        .or_else(|| {
            icon_bases()
                .into_iter()
                .chain(std::iter::once(PathBuf::from("/usr/share/pixmaps")))
                .find_map(|base| {
                    ["png", "svg", "xpm"].iter().find_map(|ext| {
                        let path = base.join(format!("{name}.{ext}"));
                        path.is_file().then_some(path)
                    })
                })
        });
    if let Ok(mut cache) = FOUND.lock() {
        cache
            .get_or_insert_with(HashMap::new)
            .insert(key, found.clone());
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> Context {
        Context {
            id: "a.desktop".into(),
            name: "Alpha".into(),
            icon: Some("alpha".into()),
            path: "/x/a.desktop".into(),
            dir: None,
            terminal: false,
        }
    }

    #[test]
    fn exec_is_split_and_its_field_codes_filled() {
        assert_eq!(
            split_exec(r#"sh -c "echo \"hi\" \$HOME" %U"#).unwrap(),
            ["sh", "-c", "echo \"hi\" $HOME", "%U"]
        );
        assert_eq!(split_exec(r#"broken "quote"#), None);
        let args = split_exec("app %i --name=%c %k %f 100%%").unwrap();
        assert_eq!(
            expand(args, &context()),
            [
                "app",
                "--icon",
                "alpha",
                "--name=Alpha",
                "/x/a.desktop",
                "100%"
            ]
        );
        // The value's own escapes come first.
        assert_eq!(unescape(r"a\sb\\c"), r"a b\c");
        assert_eq!(split_list(r"a;b\;c;;"), ["a", "b;c"]);
    }

    #[test]
    fn languages_go_from_most_to_least_exact() {
        assert_eq!(
            locales_of("sr_RS.UTF-8@latin"),
            ["sr_RS@latin", "sr_RS", "sr@latin", "sr"]
        );
        assert_eq!(locales_of("ja_JP.UTF-8"), ["ja_JP", "ja"]);
        assert!(locales_of("C.UTF-8").is_empty());
    }

    #[test]
    fn entries_are_read_and_filtered() {
        let text = "[Desktop Entry]\nType=Application\nName=Alpha Editor\nName[ja]=アルファ\nGenericName=Text Editor\nKeywords=write;notes;\nExec=alpha %F\nIcon=alpha\nActions=new;\n\n[Desktop Action new]\nName=New Window\nExec=alpha --new\n";
        let ja = vec!["ja_JP".to_owned(), "ja".to_owned()];
        let app = read_app("a.desktop", Path::new("/a.desktop"), text, &[], &ja).unwrap();
        assert_eq!(app.name, "アルファ");
        assert_eq!(app.keywords, ["write", "notes"]);
        assert_eq!(app.actions[0].name, "New Window");
        let app = read_app("a.desktop", Path::new("/a.desktop"), text, &[], &[]).unwrap();
        assert_eq!(app.name, "Alpha Editor");
        let kde = text.replace("Type=Application", "Type=Application\nOnlyShowIn=KDE;");
        assert!(read_app("a", Path::new("/a"), &kde, &["GNOME".into()], &[]).is_none());
        assert!(read_app("a", Path::new("/a"), &kde, &["KDE".into()], &[]).is_some());
        let not = text.replace("Type=Application", "Type=Application\nNotShowIn=GNOME;");
        assert!(read_app("a", Path::new("/a"), &not, &["GNOME".into()], &[]).is_none());
        let hidden = text.replace("Type=Application", "Type=Application\nNoDisplay=true");
        assert!(read_app("a", Path::new("/a"), &hidden, &[], &[]).is_none());
        let link = text.replace("Type=Application", "Type=Link");
        assert!(read_app("a", Path::new("/a"), &link, &[], &[]).is_none());
    }

    #[test]
    fn searches_rank_the_name_first() {
        let make = |name: &str, generic: Option<&str>, keywords: &[&str]| {
            let text = format!(
                "[Desktop Entry]\nType=Application\nName={name}\nExec=x\n{}Keywords={};\n",
                generic
                    .map(|g| format!("GenericName={g}\n"))
                    .unwrap_or_default(),
                keywords.join(";")
            );
            read_app("x.desktop", Path::new("/x"), &text, &[], &[]).unwrap()
        };
        let vsc = make("Visual Studio Code", Some("Text Editor"), &["ide"]);
        assert_eq!(rank(&vsc, "vis"), Some(0));
        assert_eq!(rank(&vsc, "stu"), Some(1));
        assert_eq!(rank(&vsc, "vsc"), Some(2));
        assert_eq!(rank(&vsc, "edi"), Some(3));
        assert_eq!(rank(&vsc, "udio"), Some(4));
        assert_eq!(rank(&vsc, "xt ed"), Some(5));
        assert_eq!(rank(&vsc, "ide"), Some(6));
        assert_eq!(rank(&vsc, "zzz"), None);
    }

    #[test]
    fn theme_directories_match_sizes() {
        let t = parse_theme(
            "[Icon Theme]\nName=T\nInherits=a,b\nDirectories=48x48/apps,scalable/apps,32x32@2/apps\n\n[48x48/apps]\nSize=48\nType=Fixed\n\n[scalable/apps]\nSize=64\nType=Scalable\nMinSize=16\nMaxSize=512\n\n[32x32@2/apps]\nSize=32\nScale=2\n",
            Vec::new(),
        );
        assert_eq!(t.inherits, ["a", "b"]);
        assert_eq!(t.dirs.len(), 3);
        assert!(t.dirs[0].matches(48) && !t.dirs[0].matches(32));
        assert!(t.dirs[1].matches(100));
        assert_eq!(t.dirs[1].extensions()[0], "svg");
        // Another scale never matches, only comes closest.
        assert!(!t.dirs[2].matches(32));
        assert_eq!(t.dirs[0].distance(40), 8);
    }
}
