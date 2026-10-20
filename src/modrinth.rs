//! Modrinth App: protect = point its wrapper hook at the shim, restore = clear it.
//! Settings live in `<data dir>/ModrinthApp/app.db` (SQLite, `settings` row id 0), also as a Flatpak.
use crate::{Instance, Issue, R, Status};
use rusqlite::Connection;
use std::path::{Path, PathBuf};

pub fn dir() -> PathBuf {
    data().join("ModrinthApp")
}

fn data() -> PathBuf {
    crate::launcher_data("ModrinthApp", "com.modrinth.ModrinthApp")
}

fn shim() -> PathBuf {
    crate::shim_in(&data())
}

/// The hook as Modrinth stores it: it runs `shlex::split(hook)`, POSIX rules, so `\` would be eaten
/// as an escape. Single quotes keep `\` and spaces; a `'` in the path becomes `'\''`.
fn hook() -> String {
    format!("'{}'", shim().to_string_lossy().replace('\'', r"'\''"))
}

pub struct Modrinth;

fn open() -> R<Connection> {
    if crate::Launcher::is_running(&Modrinth) {
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

/// Instances with their own wrapper hook, which replaces ours.
// ponytail: matches the overrides JSON as text; parse it once its format is known.
fn own_wrapper(db: &Connection) -> R<Vec<String>> {
    let mut q = db.prepare(
        "SELECT i.path FROM instances i JOIN instance_launch_overrides o ON o.instance_id = i.id
         WHERE CAST(o.overrides AS TEXT) LIKE '%wrapper%'",
    )?;
    Ok(q.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?)
}

impl crate::Launcher for Modrinth {
    fn id(&self) -> &'static str {
        "modrinth"
    }
    fn name(&self) -> &'static str {
        "Modrinth App"
    }
    fn installed(&self) -> bool {
        dir().join("app.db").exists()
    }
    fn is_running(&self) -> bool {
        crate::sys::running(&["Modrinth App.exe", "modrinth-app-wrapped"])
    }
    fn localhost(&self) -> bool {
        true // the live log in Modrinth comes over a localhost socket
    }

    /// Most recently played instances first.
    fn status(&self) -> R<Status> {
        let db = Connection::open_with_flags(dir().join("app.db"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let current: Option<String> = db.query_row("SELECT hook_wrapper FROM settings WHERE id = 0", [], |r| r.get(0))?;
        let protected = current.is_some_and(|w| w == hook());
        let profiles = profiles(&db)?;
        let own = own_wrapper(&db)?;
        let mut q = db.prepare(
            "SELECT i.name, i.path, cs.game_version, cs.loader FROM instances i
             LEFT JOIN instance_content_sets cs ON cs.id = i.applied_content_set_id
             ORDER BY i.last_played DESC NULLS LAST, i.name",
        )?;
        let instances = q
            .query_map([], |r| {
                let key: String = r.get(1)?;
                Ok(Instance {
                    name: r.get(0)?,
                    game_dir: profiles.join(&key),
                    game_version: r.get(2)?,
                    loader: r.get(3)?,
                    issue: own.contains(&key).then_some(Issue::Bypass),
                    key,
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok(Status { protected, instances })
    }

    /// Set the shim as Modrinth's wrapper hook and prepare a container per instance (admin).
    fn protect(&self) -> R<Vec<String>> {
        let db = open()?;
        let current: Option<String> = db.query_row("SELECT hook_wrapper FROM settings WHERE id = 0", [], |r| r.get(0))?;
        if let Some(w) = current.filter(|w| !w.is_empty() && *w != hook() && Path::new(w) != shim()) {
            return Err(format!("Modrinth already has a wrapper hook ({w}), Guarden won't replace it").into());
        }
        crate::shim_for(&data())?;
        db.execute("UPDATE settings SET hook_wrapper = ?1 WHERE id = 0", [hook()])?;
        let mut warn: Vec<String> = own_wrapper(&db)?.into_iter().map(|p| format!("instance {p} has its own wrapper hook: NOT protected")).collect();
        warn.extend(crate::prepare(self)?);
        Ok(warn)
    }

    /// Clear the wrapper hook if it is still ours.
    // ponytail: ACEs and container profiles stay; revoke them once grants are recorded.
    fn restore(&self) -> R<()> {
        let db = open()?;
        // also the unquoted value written by the first version, which Modrinth couldn't run
        db.execute(
            "UPDATE settings SET hook_wrapper = NULL WHERE id = 0 AND hook_wrapper IN (?1, ?2)",
            [hook(), shim().to_string_lossy().into_owned()],
        )?;
        Ok(())
    }
}
