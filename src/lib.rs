pub mod curseforge;
pub mod modrinth;
pub mod policy;
pub mod prism;
pub mod vanilla;

/// Everything OS-specific: the sandbox, processes, elevation.
#[cfg(windows)]
#[path = "windows/mod.rs"]
pub mod sys;
#[cfg(target_os = "linux")]
#[path = "linux/mod.rs"]
pub mod sys;

use std::path::{Path, PathBuf};

pub type R<T> = Result<T, Box<dyn std::error::Error>>;

fn env_path(var: &str) -> Option<PathBuf> {
    std::env::var_os(var).filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// `%APPDATA%\Guarden`: shims, real.txt files, logs. The container never gets access here.
pub fn root() -> PathBuf {
    sys::data_dir().join("Guarden")
}

/// The shim launchers run, in `<data>\Guarden` (no container can write there): `data` is
/// `sys::data_dir()`, or a Flatpak launcher's own (see `launcher_data`). A copy of the one shipped
/// next to the app, so a launcher never points into the install dir that updates replace.
pub fn shim_in(data: &Path) -> PathBuf {
    data.join("Guarden").join(format!("guarden-shim{}", std::env::consts::EXE_SUFFIX))
}

pub fn installed_shim() -> PathBuf {
    shim_in(&sys::data_dir())
}

/// Data dir of a launcher that keeps its files in `<data dir>/<name>`: the usual one, or else the one
/// of its Flatpak `id` (Linux), `~/.var/app/<id>/data`. A Flatpak sees only its own dirs.
pub fn launcher_data(name: &str, id: &str) -> PathBuf {
    let flatpak = sys::home().join(".var/app").join(id).join("data");
    if !sys::data_dir().join(name).exists() && flatpak.join(name).exists() { flatpak } else { sys::data_dir() }
}

/// The shim for a launcher whose data dir is `data`: copied there from the installed one if needed.
pub fn shim_for(data: &Path) -> std::io::Result<PathBuf> {
    let shim = shim_in(data);
    if shim != installed_shim() {
        std::fs::create_dir_all(shim.parent().unwrap_or(data))?;
        std::fs::copy(installed_shim(), &shim)?;
    }
    Ok(shim)
}

/// Copy `shipped` over the installed shim, its copies in Flatpaks and every fake java, where they
/// differ. `create`: install the shim if missing (protecting a launcher); otherwise only refresh
/// existing copies (app start, after an update). Fails while a game runs (the shim is in use); the
/// next start retries.
pub fn update_shims(shipped: &Path, create: bool) -> std::io::Result<()> {
    let new = std::fs::read(shipped)?;
    let fakes = std::fs::read_dir(root().join("java")).into_iter().flatten().flatten();
    let fakes = fakes.flat_map(|d| sys::JAVAS.map(|n| d.path().join("bin").join(n)));
    let flatpaks = std::fs::read_dir(sys::home().join(".var/app")).into_iter().flatten().flatten();
    let flatpaks = flatpaks.map(|d| shim_in(&d.path().join("data")));
    for dst in std::iter::once(installed_shim()).chain(flatpaks).chain(fakes) {
        match std::fs::read(&dst) {
            Ok(old) if old == new => {}
            Err(_) if !create || dst != installed_shim() => {}
            _ => {
                std::fs::create_dir_all(dst.parent().unwrap_or(&root()))?;
                std::fs::copy(shipped, &dst)?; // keeps it executable (Linux)
            }
        }
    }
    Ok(())
}

/// Fake java for launchers without a wrapper hook: `java\<id>\bin\<sys::JAVAS>` copies of the shim,
/// `<id>\real.txt` naming the real JAVA_HOME, or `auto` to pick the game version's Mojang runtime.
/// Returns its `bin\<sys::JAVA>`. Needs the shim installed (`update_shims`).
pub fn fake_java(home: Option<&Path>) -> std::io::Result<PathBuf> {
    // components(): drops a trailing `\` (tab-completion adds one), which would change the id.
    let home: Option<PathBuf> = home.map(|h| h.components().collect());
    let real = home.as_ref().map_or("auto".into(), |h| h.to_string_lossy().into_owned());
    let dir = root().join("java").join(if home.is_some() { short_hash(&real) } else { "auto".into() });
    std::fs::create_dir_all(dir.join("bin"))?;
    for name in sys::JAVAS {
        std::fs::copy(installed_shim(), dir.join("bin").join(name))?;
    }
    std::fs::write(dir.join("real.txt"), real)?;
    Ok(dir.join("bin").join(sys::JAVA))
}

/// Is `p` one of our fake javas? (Then a launcher setting pointing there is ours to restore.)
pub fn is_fake_java(p: &Path) -> bool {
    p.starts_with(root().join("java"))
}

/// Why an instance isn't sandboxed even with its launcher protected.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Issue {
    /// It launches without Guarden: its own launch settings, or created after protecting. Protecting
    /// the launcher again fixes it when Guarden can (it won't replace someone else's wrapper).
    Bypass,
    /// Its game dir is the launcher's own folder (vanilla `.minecraft`): the game could rewrite what the
    /// launcher runs. Fixed by giving it its own folder (`vanilla::separate`).
    Shared,
}

pub struct Instance {
    /// The launcher's id for it (Modrinth path, Prism folder, vanilla profile id).
    pub key: String,
    pub name: String,
    pub game_version: Option<String>,
    /// "fabric", "forge", "neoforge", "quilt" or None/"vanilla".
    pub loader: Option<String>,
    /// What the launcher passes as `--gameDir`, so `container_name(game_dir)` is the instance's container.
    pub game_dir: PathBuf,
    pub issue: Option<Issue>,
}

pub struct Status {
    pub protected: bool,
    pub instances: Vec<Instance>,
}

/// A launcher Guarden protects. Protect/restore need it closed: launchers keep their settings in
/// memory and would overwrite ours.
pub trait Launcher: Sync {
    fn id(&self) -> &'static str;
    fn name(&self) -> &'static str;
    fn installed(&self) -> bool;
    fn is_running(&self) -> bool;
    /// Read-only, fine while the launcher runs.
    fn status(&self) -> R<Status>;
    /// Route its launches through the shim (installed by the caller). Returns warnings.
    fn protect(&self) -> R<Vec<String>>;
    fn restore(&self) -> R<()>;
    /// Its containers need a localhost exemption (admin), the launcher reading the game's log over a
    /// socket (Modrinth). The others read stdout and protect without admin.
    fn localhost(&self) -> bool {
        false
    }
}

pub fn launchers() -> [&'static dyn Launcher; 4] {
    [&modrinth::Modrinth, &prism::Prism, &curseforge::CurseForge, &vanilla::Vanilla]
}

pub fn launcher(id: &str) -> Option<&'static dyn Launcher> {
    launchers().into_iter().find(|l| l.id() == id)
}

/// Create each instance's container with its localhost exemption (admin), for launchers that need it.
/// Doesn't touch the launcher's settings, so it works while it's open: also how a new instance gets fixed.
pub fn prepare(l: &dyn Launcher) -> R<Vec<String>> {
    let mut warn = vec![];
    if l.localhost() {
        for i in l.status()?.instances {
            let name = container_name(&i.game_dir);
            if let Err(e) = sys::prepare(&name) {
                warn.push(format!("{}: {e}", i.game_dir.display()));
            }
        }
    }
    Ok(warn)
}

/// Write a launcher's main settings file (`write_atomic`). The first time, keeps the original as
/// `<Guarden root>\backup\<name>`.
pub fn write_settings(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let backup = root().join("backup");
    let keep = backup.join(path.file_name().unwrap_or_default());
    if !keep.exists() && path.exists() {
        std::fs::create_dir_all(&backup)?;
        std::fs::copy(path, &keep)?;
    }
    write_atomic(path, data)
}

/// Write through a temp file and a rename: a crash leaves the old file or the new, never half of one.
pub fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("guarden-tmp");
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)
}

/// Dirs that must never be exposed to the game, directly or through a grant on an ancestor.
// Each launcher root: where its tokens and the settings it executes live. Instances inside are fine.
pub fn deny_list() -> Vec<PathBuf> {
    let mut v = sys::private_dirs();
    v.extend([modrinth::dir(), prism::dir(), curseforge::dir(), vanilla::dir()]);
    v.push(curseforge::minecraft_root()); // its Install\ (java, Mojang launcher); Instances\ below are fine
    v.push(root());
    v.retain(|d| d.is_absolute()); // an unset env var
    v
}

/// FNV-1a over the lowercased string. Unlike DefaultHasher it is stable across Rust
/// releases, which matters because these ids name on-disk dirs and AppContainer profiles.
pub fn short_hash(s: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.to_lowercase().bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// AppContainer of the instance at `game_dir`: one per instance, so instances can't read each other.
/// `/` and `\` count the same: Prism passes `C:/...`, its config says `C:\...`.
pub fn container_name(game_dir: &Path) -> String {
    format!("Guarden.{}", short_hash(game_dir.to_string_lossy().replace('/', "\\").trim_end_matches('\\')))
}

/// One game launch, read back from the shim's log.
#[derive(Debug, PartialEq)]
pub struct Launch {
    pub container: String,
    pub start: u64,
    pub end: Option<u64>,
    pub code: Option<u32>,
}

/// Game launches in `logs\shim.log`, oldest first. Launcher probes (`Guarden.probe`) are left out.
pub fn launches() -> Vec<Launch> {
    parse_log(&std::fs::read_to_string(root().join("logs").join("shim.log")).unwrap_or_default())
}

/// Start lines are `[ts] Guarden.x <java> rw=...`, end lines `[ts] Guarden.x exited with 0x..`.
fn parse_log(log: &str) -> Vec<Launch> {
    let mut out: Vec<Launch> = vec![];
    for line in log.lines() {
        let Some((ts, msg)) = line.strip_prefix('[').and_then(|l| l.split_once("] ")) else { continue };
        let (Ok(ts), Some((container, rest))) = (ts.parse(), msg.split_once(' ')) else { continue };
        if !container.starts_with("Guarden.") || container == "Guarden.probe" {
            continue;
        }
        match rest.strip_prefix("exited with 0x") {
            Some(code) => {
                if let Some(l) = out.iter_mut().rev().find(|l| l.container == container && l.end.is_none()) {
                    l.end = Some(ts);
                    l.code = u32::from_str_radix(code.trim(), 16).ok();
                }
            }
            None => out.push(Launch { container: container.into(), start: ts, end: None, code: None }),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launches_from_log() {
        let log = "[10] Guarden.probe C:\\j\\javaw.exe rw=[] ro=[]\n\
                   [11] java exited with 0x0\n\
                   [20] Guarden.aa C:\\j\\javaw.exe rw=[\"C:\\\\g\"] ro=[]\n\
                   [21] args: [\"x\"]\n\
                   [25] Guarden.bb C:\\j\\javaw.exe rw=[] ro=[]\n\
                   [30] Guarden.bb exited with 0xfffffffd\n\
                   [90] Guarden.aa exited with 0x0\n\
                   [95] guarden: refusing to expose C:\\\n\
                   [99] Guarden.aa C:\\j\\javaw.exe rw=[] ro=[]\n";
        assert_eq!(
            parse_log(log),
            [
                Launch { container: "Guarden.aa".into(), start: 20, end: Some(90), code: Some(0) },
                Launch { container: "Guarden.bb".into(), start: 25, end: Some(30), code: Some(0xfffffffd) },
                Launch { container: "Guarden.aa".into(), start: 99, end: None, code: None },
            ]
        );
    }

    #[test]
    fn hash_is_stable_and_case_insensitive() {
        assert_eq!(short_hash(r"C:\Game"), short_hash(r"c:\game"));
        assert_eq!(short_hash(""), "cbf29ce484222325");
        assert_eq!(container_name(Path::new("C:/G/x/")), container_name(Path::new(r"c:\G\x")));
    }
}
