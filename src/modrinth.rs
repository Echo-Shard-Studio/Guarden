//! Modrinth App: protect = point its wrapper hook at the shim, restore = clear it.
//! Settings live in `%APPDATA%\ModrinthApp\app.db` (SQLite, `settings` row id 0).
use rusqlite::Connection;
use std::path::{Path, PathBuf};

type R<T> = Result<T, Box<dyn std::error::Error>>;

fn dir() -> PathBuf {
    crate::env_path("APPDATA").unwrap_or_default().join("ModrinthApp")
}

fn shim() -> PathBuf {
    crate::installed_shim()
}

/// The hook as Modrinth stores it: it runs `shlex::split(hook)`, POSIX rules, so `\` would be eaten
/// as an escape. Single quotes keep `\` and spaces; a `'` in the path becomes `'\''`.
fn hook() -> String {
    format!("'{}'", shim().to_string_lossy().replace('\'', r"'\''"))
}

pub fn installed() -> bool {
    dir().join("app.db").exists()
}

/// Modrinth running? It keeps its settings in memory and would overwrite ours on its next save.
// ponytail: process name check; a renamed Modrinth exe slips through.
pub fn is_running() -> bool {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000; // called from the GUI: no console flash
    std::process::Command::new("tasklist")
        .args(["/FI", "IMAGENAME eq Modrinth App.exe", "/NH"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("Modrinth App.exe"))
}

fn open() -> R<Connection> {
    if is_running() {
        return Err("close Modrinth first, it would overwrite its settings".into());
    }
    let db = dir().join("app.db");
    if !db.exists() {
        return Err(format!("Modrinth not found ({} missing)", db.display()).into());
    }
    Ok(Connection::open(db)?)
}

/// Where instances live: `<custom_dir or ModrinthApp>\profiles`.
fn profiles(db: &Connection) -> R<PathBuf> {
    let custom: Option<String> = db.query_row("SELECT custom_dir FROM settings WHERE id = 0", [], |r| r.get(0))?;
    Ok(custom.filter(|c| !c.is_empty()).map(PathBuf::from).unwrap_or_else(dir).join("profiles"))
}

pub struct Instance {
    pub name: String,
    pub game_version: Option<String>,
    pub loader: Option<String>,
    /// What Modrinth passes as `--gameDir`, so `container_name(game_dir)` is the instance's container.
    pub game_dir: PathBuf,
}

pub struct Status {
    pub protected: bool,
    pub instances: Vec<Instance>,
}

/// Read-only look at Modrinth, fine while it runs. Most recently played instances first.
pub fn status() -> R<Status> {
    let db = Connection::open_with_flags(dir().join("app.db"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let current: Option<String> = db.query_row("SELECT hook_wrapper FROM settings WHERE id = 0", [], |r| r.get(0))?;
    let protected = current.is_some_and(|w| w == hook());
    let profiles = profiles(&db)?;
    let mut q = db.prepare(
        "SELECT i.name, i.path, cs.game_version, cs.loader FROM instances i
         LEFT JOIN instance_content_sets cs ON cs.id = i.applied_content_set_id
         ORDER BY i.last_played DESC NULLS LAST, i.name",
    )?;
    let instances = q
        .query_map([], |r| {
            Ok(Instance { name: r.get(0)?, game_dir: profiles.join(r.get::<_, String>(1)?), game_version: r.get(2)?, loader: r.get(3)? })
        })?
        .collect::<Result<_, _>>()?;
    Ok(Status { protected, instances })
}

/// Install the shim, set it as Modrinth's wrapper hook, and prepare a container per instance.
/// Returns warnings: instances left unprotected or not prepared.
pub fn protect(shim_src: &Path) -> R<Vec<String>> {
    let db = open()?;
    let shim = shim();
    let current: Option<String> = db.query_row("SELECT hook_wrapper FROM settings WHERE id = 0", [], |r| r.get(0))?;
    if let Some(w) = current.filter(|w| !w.is_empty() && *w != hook() && Path::new(w) != shim) {
        return Err(format!("Modrinth already has a wrapper hook ({w}), Guarden won't replace it").into());
    }
    std::fs::create_dir_all(crate::root())?;
    std::fs::copy(shim_src, &shim)?;
    db.execute("UPDATE settings SET hook_wrapper = ?1 WHERE id = 0", [hook()])?;

    let mut warn = vec![];
    // ponytail: matches the overrides JSON as text; parse it once its format is known.
    let mut q = db.prepare(
        "SELECT i.name FROM instances i JOIN instance_launch_overrides o ON o.instance_id = i.id
         WHERE CAST(o.overrides AS TEXT) LIKE '%wrapper%'",
    )?;
    for name in q.query_map([], |r| r.get::<_, String>(0))? {
        warn.push(format!("instance \"{}\" has its own wrapper hook: NOT protected", name?));
    }

    warn.extend(prepare()?);
    Ok(warn)
}

/// Create each instance's container with its localhost exemption (admin). Doesn't touch Modrinth's
/// settings, so it works while Modrinth is open: it's also how a new instance gets fixed.
pub fn prepare() -> R<Vec<String>> {
    let mut warn = vec![];
    for i in status()?.instances {
        let name = crate::container_name(&i.game_dir);
        match crate::sandbox::prepare(&name) {
            Ok(()) => println!("{name}  {}", i.game_dir.display()),
            Err(e) => warn.push(format!("{}: {e}", i.game_dir.display())),
        }
    }
    Ok(warn)
}

/// Clear the wrapper hook if it is still ours.
// ponytail: ACEs and container profiles stay; revoke them once grants are recorded.
pub fn restore() -> R<()> {
    let db = open()?;
    // also the unquoted value written by the first version, which Modrinth couldn't run
    db.execute(
        "UPDATE settings SET hook_wrapper = NULL WHERE id = 0 AND hook_wrapper IN (?1, ?2)",
        [hook(), shim().to_string_lossy().into_owned()],
    )?;
    Ok(())
}
