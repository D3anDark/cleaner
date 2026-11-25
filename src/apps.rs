use crate::matcher::{Descriptor, normalize};
use std::collections::HashSet;
#[cfg(target_os = "macos")]
use std::os::macos::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// Apps sit one or two folders down (`/Applications/Utilities`, `~/Applications/JetBrains Toolbox`).
#[cfg(target_os = "macos")]
const MAX_ROOT_DEPTH: usize = 4;
#[cfg(target_os = "macos")]
const SF_RESTRICTED: u32 = 0x0008_0000;
#[cfg(target_os = "macos")]
const HELPER_DIRS: &[&str] = &["Library/LoginItems", "Library/SystemExtensions", "Library/Spotlight", "Library/QuickLook", "PlugIns", "Extensions", "XPCServices", "Frameworks"];

#[derive(Clone, Debug)]
pub struct App {
    pub path: PathBuf,
    pub bundle_id: Option<String>,
    pub name: String,
    pub bundle_name: Option<String>,
    pub executable: Option<String>,
    /// Bundle ids of login items, plug-ins and XPC services: their files belong to this app.
    pub helper_ids: Vec<String>,
    /// Apple's own SIP-protected bundles, never removable.
    pub is_system: bool,
    pub version: Option<String>,
    /// Bundle size on disk, None until measured after listing.
    pub size: Option<u64>,
    /// Seconds since the epoch the bundle was created here, close to its install date.
    pub installed: i64,
}

impl App {
    pub fn descriptor(&self) -> Descriptor {
        let file_name = self.path.file_stem().map(|stem| stem.to_string_lossy().into_owned()).unwrap_or_default();
        let ids = self.bundle_id.iter().chain(&self.helper_ids).map(String::as_str);
        let names = [Some(self.name.as_str()), self.bundle_name.as_deref(), Some(file_name.as_str()), self.executable.as_deref()];
        Descriptor::new(ids, names.into_iter().flatten())
    }
}

#[cfg(target_os = "macos")]
pub fn installed() -> Vec<App> {
    let home = std::env::var("HOME").unwrap_or_default();
    let roots = [
        PathBuf::from("/Applications"),
        PathBuf::from(&home).join("Applications"),
        PathBuf::from("/System/Applications"),
        PathBuf::from("/System/Library/CoreServices/Applications"),
        PathBuf::from("/Network/Applications"),
    ];
    let mut bundles = Vec::new();
    for root in &roots {
        find_bundles(root, 0, &mut bundles);
    }
    let mut seen = HashSet::new();
    let mut apps: Vec<App> =
        bundles.into_iter().filter(|path| seen.insert(std::fs::canonicalize(path).unwrap_or_else(|_| path.clone()).to_string_lossy().to_lowercase())).filter_map(|path| read(&path)).collect();
    apps.sort_by_cached_key(|app| app.name.to_lowercase());
    apps
}

#[cfg(target_os = "macos")]
/// Every `.app` at or below `folder`, never descending into a bundle.
fn find_bundles(folder: &Path, depth: usize, found: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(folder) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        if path.extension().is_some_and(|ext| ext == "app") {
            found.push(path);
        } else if depth + 1 < MAX_ROOT_DEPTH && entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            find_bundles(&path, depth + 1, found);
        }
    }
}

#[cfg(target_os = "macos")]
pub fn read(path: &Path) -> Option<App> {
    if !path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("app")) || !path.is_dir() || is_in_trash(&path.to_string_lossy()) {
        return None;
    }
    let info = info_plist(path);
    let field = |key: &str| info.as_ref().and_then(|dict| dict.get(key)?.as_string()).filter(|value| !value.is_empty()).map(str::to_owned);
    let bundle_id = field("CFBundleIdentifier");
    let bundle_name = field("CFBundleName");
    let file_name = path.file_stem()?.to_string_lossy().into_owned();
    let name = field("CFBundleDisplayName").or_else(|| bundle_name.clone()).unwrap_or(file_name);
    Some(App {
        helper_ids: helper_ids(path, bundle_id.as_deref()),
        is_system: is_system_bundle(path),
        bundle_name: bundle_name.filter(|bundle_name| *bundle_name != name),
        executable: field("CFBundleExecutable"),
        version: field("CFBundleShortVersionString"),
        size: None,
        installed: std::fs::metadata(path).map_or(0, |meta| meta.st_birthtime()),
        path: path.to_path_buf(),
        bundle_id,
        name,
    })
}

#[cfg(target_os = "macos")]
fn info_plist(bundle: &Path) -> Option<plist::Dictionary> {
    let value = plist::Value::from_file(bundle.join("Contents/Info.plist")).ok()?;
    value.into_dictionary()
}

#[cfg(target_os = "macos")]
/// Everything under `/System/`, plus built-ins Apple links into `/Applications` from a cryptex
/// (Safari). The flag sits on the link itself; an unreadable bundle counts as system.
fn is_system_bundle(path: &Path) -> bool {
    path.starts_with("/System/") || std::fs::symlink_metadata(path).map_or(true, |meta| meta.st_flags() & SF_RESTRICTED != 0)
}

#[cfg(target_os = "macos")]
fn helper_ids(bundle: &Path, primary: Option<&str>) -> Vec<String> {
    let mut ids = Vec::new();
    for sub in HELPER_DIRS {
        let Ok(entries) = std::fs::read_dir(bundle.join("Contents").join(sub)) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let ext = path.extension().map(|ext| ext.to_string_lossy().to_lowercase()).unwrap_or_default();
            if *sub == "Frameworks" && ext != "app" && ext != "xpc" {
                continue;
            }
            let id = info_plist(&path).and_then(|dict| dict.get("CFBundleIdentifier")?.as_string().map(str::to_owned));
            if let Some(id) = id
                && Some(id.as_str()) != primary
                && !ids.contains(&id)
            {
                ids.push(id);
            }
        }
    }
    ids.sort();
    ids
}

/// What the size column measures: the bundle on macOS. A Linux launcher is a few bytes, so there it is
/// the flatpak or the program the launcher starts, and the launcher when neither is found.
#[cfg(target_os = "macos")]
pub fn payload(app: &App) -> PathBuf {
    app.path.clone()
}

#[cfg(target_os = "linux")]
pub fn payload(app: &App) -> PathBuf {
    let home = crate::scan::home();
    let flatpak = app.bundle_id.iter().flat_map(|id| [Path::new("/var/lib/flatpak/app").join(id), home.join(".local/share/flatpak/app").join(id)]).find(|path| path.is_dir());
    let program = || launcher_program(&app.path).and_then(|program| which(&program)).and_then(|path| std::fs::canonicalize(path).ok()).filter(|path| path.is_file() && !is_wrapper(path));
    flatpak.or_else(program).unwrap_or_else(|| app.path.clone())
}

/// Folders of desktop entries, the user's first: an entry id found twice keeps the first, as menus do.
#[cfg(target_os = "linux")]
fn desktop_folders() -> Vec<PathBuf> {
    let home = crate::scan::home();
    let data_dirs = std::env::var("XDG_DATA_DIRS").ok().filter(|dirs| !dirs.is_empty()).unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    let mut roots = vec![home.join(".local/share")];
    roots.extend(std::env::split_paths(&data_dirs));
    roots.extend([home.join(".local/share/flatpak/exports/share"), "/var/lib/flatpak/exports/share".into(), "/var/lib/snapd/desktop".into()]);
    let mut folders: Vec<PathBuf> = Vec::new();
    for folder in roots.into_iter().map(|root| root.join("applications")) {
        if !folders.contains(&folder) {
            folders.push(folder);
        }
    }
    folders
}

/// Every app with a menu entry. Package managers own the ones outside the home folder, so those count
/// as system: their leftovers can go, the app itself goes with `dnf`, `apt`, `flatpak` or `snap`.
#[cfg(target_os = "linux")]
pub fn installed() -> Vec<App> {
    let mut seen = HashSet::new();
    let mut apps = Vec::new();
    for folder in desktop_folders() {
        let Ok(entries) = std::fs::read_dir(&folder) else { continue };
        let mut paths: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).filter(|path| path.extension().is_some_and(|ext| ext == "desktop")).collect();
        paths.sort();
        for path in paths {
            if seen.insert(path.file_name().map(std::ffi::OsStr::to_os_string))
                && let Some(app) = read(&path)
            {
                apps.push(app);
            }
        }
    }
    apps.sort_by_cached_key(|app| app.name.to_lowercase());
    apps
}

#[cfg(target_os = "linux")]
pub fn read(path: &Path) -> Option<App> {
    let entry = desktop_entry(path)?;
    let field = |key: &str| entry.iter().find(|(name, _)| name == key).map(|(_, value)| value.as_str()).filter(|value| !value.is_empty());
    if field("Type") != Some("Application") || field("NoDisplay") == Some("true") || field("Hidden") == Some("true") || is_in_trash(&path.to_string_lossy()) {
        return None;
    }
    let stem = path.file_stem()?.to_string_lossy().into_owned();
    // A desktop file id is reverse DNS when the app has one (org.kde.dolphin), as is every flatpak's.
    let reverse_dns = stem.matches('.').count() >= 2 && !stem.contains(char::is_whitespace);
    let bundle_id = field("X-Flatpak").map(str::to_owned).or(reverse_dns.then(|| stem.clone()));
    let name = field("Name").map_or(stem, str::to_owned);
    let executable = field("Exec").and_then(exec_program).filter(|program| !is_wrapper(Path::new(program))).and_then(|program| Some(Path::new(&program).file_name()?.to_string_lossy().into_owned()));
    let meta = std::fs::metadata(path).ok();
    let created = meta.and_then(|meta| meta.created().or_else(|_| meta.modified()).ok());
    Some(App {
        is_system: !path.starts_with(crate::scan::home().join(".local/share/applications")),
        bundle_name: field("StartupWMClass").filter(|class| *class != name && !is_wrapper(Path::new(class))).map(str::to_owned),
        executable,
        helper_ids: vec![],
        version: None,
        size: None,
        installed: created.and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |elapsed| elapsed.as_secs() as i64),
        path: path.to_path_buf(),
        bundle_id,
        name,
    })
}

/// The unlocalized keys of the `[Desktop Entry]` group, values unescaped.
#[cfg(target_os = "linux")]
fn desktop_entry(path: &Path) -> Option<Vec<(String, String)>> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut group = "";
    let mut entry = Vec::new();
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            group = line;
        } else if group == "[Desktop Entry]"
            && let Some((key, value)) = line.split_once('=')
            && !key.contains('[')
        {
            entry.push((key.trim().to_owned(), unescape(value.trim())));
        }
    }
    Some(entry)
}

/// The spec's value escapes; any other backslash is left for the Exec quoting rules.
#[cfg(target_os = "linux")]
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
            Some(other) => out.extend(['\\', other]),
            None => out.push('\\'),
        }
    }
    out
}

/// The program an Exec line starts, as written: quotes removed, `env` and its variables skipped.
#[cfg(target_os = "linux")]
pub fn exec_program(exec: &str) -> Option<String> {
    let mut words = Vec::new();
    let (mut word, mut quoted, mut chars) = (None::<String>, false, exec.chars());
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                quoted = !quoted;
                word.get_or_insert_default();
            }
            '\\' if quoted => word.get_or_insert_default().extend(chars.next()),
            // Not in the spec, but AppImage tools write `Exec=env A=1 '/path/app.appimage'` and launchers accept it.
            '\'' if !quoted => word.get_or_insert_default().extend(chars.by_ref().take_while(|&c| c != '\'')),
            c if c.is_whitespace() && !quoted => words.extend(word.take()),
            c => word.get_or_insert_default().push(c),
        }
    }
    words.extend(word);
    let mut words = words.into_iter().peekable();
    if words.peek().is_some_and(|first| Path::new(first).file_name().is_some_and(|name| name == "env")) {
        words.next();
        while words.next_if(|word| word.contains('=') || word.starts_with('-')).is_some() {}
    }
    words.next().filter(|program| !program.is_empty())
}

/// The Exec program of a launcher file.
#[cfg(target_os = "linux")]
pub fn launcher_program(desktop: &Path) -> Option<String> {
    let entry = desktop_entry(desktop)?;
    entry.iter().find(|(key, _)| key == "Exec").and_then(|(_, exec)| exec_program(exec))
}

/// A program as the session would find it: an absolute path as is, a bare name on PATH.
#[cfg(target_os = "linux")]
pub fn which(program: &str) -> Option<PathBuf> {
    if program.starts_with('/') {
        return std::fs::symlink_metadata(program).is_ok().then(|| PathBuf::from(program));
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|folder| folder.join(program)).find(|candidate| !program.contains('/') && candidate.is_file())
}

/// Programs that start other programs: their name says nothing about the app, and `steam` or
/// `flatpak` as a leftover name would take the whole Steam library or every flatpak.
#[cfg(target_os = "linux")]
fn is_wrapper(program: &Path) -> bool {
    const WRAPPERS: &[&str] = &[
        "env",
        "flatpak",
        "snap",
        "sh",
        "bash",
        "zsh",
        "dash",
        "gtk-launch",
        "gio",
        "xdg-open",
        "kioclient",
        "kioclient5",
        "kioclient6",
        "wine",
        "wine64",
        "steam",
        "lutris",
        "heroic",
        "python",
        "python3",
        "java",
        "node",
        "electron",
        "mono",
        "dotnet",
        "appimage-run",
        "sudo",
        "pkexec",
        "gamemoderun",
        "prime-run",
        "firejail",
        "bwrap",
    ];
    program.file_name().is_some_and(|name| WRAPPERS.contains(&name.to_string_lossy().to_lowercase().as_str()))
}

/// Trashed files are already on their way out and never count as installed or as leftovers.
/// `~/.local/share/Trash` and `<volume>/.Trash-<uid>` are the freedesktop Trash folders.
pub fn is_in_trash(path: &str) -> bool {
    path.contains("/.Trash/") || path.contains("/.Trashes/") || path.ends_with("/.Trash") || path.ends_with("/.Trashes") || path.contains("/.local/share/Trash/") || path.contains("/.Trash-")
}

/// Who owns an identifier or a folder name, for the orphan scan.
pub struct Index {
    ids: HashSet<String>,
    names: HashSet<String>,
}

impl Index {
    pub fn new(apps: &[App]) -> Self {
        let mut index = Index { ids: HashSet::new(), names: HashSet::new() };
        for app in apps {
            index.ids.extend(app.bundle_id.iter().chain(&app.helper_ids).map(|id| id.to_lowercase()));
            index.names.extend([Some(&app.name), app.executable.as_ref()].into_iter().flatten().map(|name| normalize(name)).filter(|name| !name.is_empty()));
        }
        index
    }

    /// Exact ids, helper children (`com.foo.Bar.helper`) and app-group prefixes (`group.com.foo.Bar`).
    pub fn owns_identifier(&self, candidate: &str) -> bool {
        let candidate = candidate.to_lowercase();
        self.ids.contains(&candidate) || self.ids.iter().any(|id| candidate.starts_with(&format!("{id}.")) || candidate.ends_with(&format!(".{id}")))
    }

    pub fn matches_name(&self, candidate: &str) -> bool {
        self.names.contains(&normalize(candidate))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_owns_helpers_and_app_groups() {
        let app = App {
            path: "/Applications/Bar.app".into(),
            bundle_id: Some("com.foo.Bar".into()),
            name: "Bar Pro".into(),
            bundle_name: None,
            executable: Some("bar".into()),
            helper_ids: vec![],
            is_system: false,
            version: None,
            size: None,
            installed: 0,
        };
        let index = Index::new(&[app]);
        assert!(index.owns_identifier("com.foo.bar"));
        assert!(index.owns_identifier("com.foo.Bar.helper"));
        assert!(index.owns_identifier("4FG648TM2A.group.com.foo.Bar"));
        assert!(!index.owns_identifier("com.foo.Barista"));
        assert!(index.matches_name("Bar-Pro"));
        assert!(!index.matches_name(""));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn exec_lines_name_the_program_they_start() {
        assert_eq!(exec_program(r#"env -i FOO=1 "/opt/My App/app" --flag %U"#).as_deref(), Some("/opt/My App/app"));
        assert_eq!(exec_program("flatpak run org.x.Y").as_deref(), Some("flatpak"));
        assert_eq!(exec_program(r#""/a/say \"hi\"" %f"#).as_deref(), Some(r#"/a/say "hi""#));
        assert_eq!(exec_program("env DESKTOPINTEGRATION=1 '/home/a/My App.appimage' %U").as_deref(), Some("/home/a/My App.appimage"));
        assert_eq!(exec_program("  "), None);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn desktop_entries_read_as_apps() {
        let root = std::env::temp_dir().join(format!("cleaner-desktop-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let entry = |file: &str, body: &str| {
            std::fs::write(root.join(file), format!("[Desktop Entry]\nType=Application\n{body}\n[Desktop Action new]\nName=Other\nExec=other\n")).unwrap();
            read(&root.join(file))
        };
        let app = entry("org.foo.Bar.desktop", "Name=Bar\\sPro\nName[pl]=Bar PL\nExec=env A=1 /opt/bar/bar-bin %U\nStartupWMClass=bar-window").unwrap();
        assert_eq!((app.name.as_str(), app.bundle_id.as_deref(), app.executable.as_deref(), app.bundle_name.as_deref()), ("Bar Pro", Some("org.foo.Bar"), Some("bar-bin"), Some("bar-window")));
        assert!(app.is_system, "outside ~/.local/share/applications a package manager owns it");
        let flatpak = entry("brave.desktop", "Name=Brave\nExec=/usr/bin/flatpak run com.brave.Browser\nX-Flatpak=com.brave.Browser").unwrap();
        assert_eq!((flatpak.bundle_id.as_deref(), flatpak.executable), (Some("com.brave.Browser"), None));
        assert!(entry("hidden.desktop", "Name=Hidden\nNoDisplay=true\nExec=hidden").is_none());
        assert!(entry("emacs-29.1.desktop", "Name=Emacs\nExec=emacs").is_some_and(|app| app.bundle_id.is_none()));
        std::fs::remove_dir_all(root).unwrap();
    }
}
