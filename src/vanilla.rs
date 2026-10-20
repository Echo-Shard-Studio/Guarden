//! Official launcher: no wrapper hook, so protect = point each installation's `javaDir` at a fake java
//! (see `fake_java`), restore = put the original back (kept in `<Guarden root>\vanilla.json`).
//! Installations live in `.minecraft/launcher_profiles*.json` (`%APPDATA%`, `~` on Linux).
//!
//! An installation whose game dir is `.minecraft` itself can't be sandboxed: the game would get write
//! access to launcher_profiles.json, whose `javaDir` the launcher runs outside any container. Those are
//! `Issue::Shared` until `separate` gives them their own folder.
use crate::{Instance, Issue, R, Status};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};

pub struct Vanilla;

pub fn dir() -> PathBuf {
    let data = crate::sys::data_dir().join(".minecraft");
    if data.exists() { data } else { crate::sys::home().join(".minecraft") }
}

/// Classic launcher, Microsoft Store launcher.
fn files() -> Vec<PathBuf> {
    ["launcher_profiles.json", "launcher_profiles_microsoft_store.json"].map(|f| dir().join(f)).into_iter().filter(|p| p.exists()).collect()
}

/// The file the launcher in use writes: the most recently modified one.
fn current() -> Option<PathBuf> {
    files().into_iter().max_by_key(|p| p.metadata().and_then(|m| m.modified()).ok())
}

fn load(p: &Path) -> R<Value> {
    Ok(serde_json::from_slice(&std::fs::read(p)?)?)
}

fn save(p: &Path, v: &Value) -> R<()> {
    Ok(crate::write_settings(p, serde_json::to_string_pretty(v)?.as_bytes())?)
}

/// `{"<profiles file name>": {"<profile id>": "<original javaDir, or empty>"}}`. A file listed here is protected.
fn backup_path() -> PathBuf {
    crate::root().join("vanilla.json")
}

fn backup() -> Map<String, Value> {
    std::fs::read(backup_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn key(p: &Path) -> String {
    p.file_name().unwrap_or_default().to_string_lossy().into_owned()
}

fn same_dir(a: &Path, b: &Path) -> bool {
    let n = |p: &Path| p.to_string_lossy().to_lowercase().replace('/', "\\").trim_end_matches('\\').to_string();
    n(a) == n(b)
}

/// The game dir the launcher passes: the profile's, or `.minecraft`.
fn game_dir(p: &Value, root: &Path) -> PathBuf {
    p["gameDir"].as_str().filter(|d| !d.is_empty()).map_or(root.into(), PathBuf::from)
}

fn ours(p: &Value) -> bool {
    p["javaDir"].as_str().is_some_and(|j| crate::is_fake_java(Path::new(j)))
}

/// Point `p`'s javaDir at a fake java for its current one (or `auto`), recording the original in `saved`.
fn wrap(id: &str, p: &mut Value, saved: &mut Map<String, Value>) -> R<()> {
    if ours(p) {
        return Ok(());
    }
    let orig = p["javaDir"].as_str().unwrap_or_default().to_string();
    // javaDir is `<home>\bin\javaw.exe`; tolerate a JAVA_HOME pasted instead.
    let home = (!orig.is_empty()).then(|| Path::new(&orig)).map(|j| if j.is_file() { j.parent().and_then(Path::parent).unwrap_or(j) } else { j });
    p["javaDir"] = json!(crate::fake_java(home)?);
    saved.insert(id.into(), json!(orig));
    Ok(())
}

/// Minecraft version and loader of version `id`, from its id and `versions\<id>\<id>.json`.
fn version_of(root: &Path, id: &str) -> (Option<String>, Option<String>) {
    let l = id.to_lowercase();
    let loader = ["fabric", "quilt", "neoforge", "forge"].into_iter().find(|n| l.contains(n));
    let parent: Option<String> = load(&root.join("versions").join(id).join(format!("{id}.json"))).ok().and_then(|v| v["inheritsFrom"].as_str().map(Into::into));
    let game = parent.or_else(|| loader.is_none().then(|| id.into())).filter(|v| !v.starts_with("latest-"));
    (game, loader.map(Into::into))
}

fn closed() -> R<()> {
    if crate::Launcher::is_running(&Vanilla) {
        return Err("close the Minecraft Launcher first, it would overwrite its settings".into());
    }
    Ok(())
}

impl crate::Launcher for Vanilla {
    fn id(&self) -> &'static str {
        "official"
    }
    fn name(&self) -> &'static str {
        "Launcher officiel"
    }
    fn installed(&self) -> bool {
        !files().is_empty()
    }
    fn is_running(&self) -> bool {
        crate::sys::running(&["MinecraftLauncher.exe", "Minecraft.exe", "minecraft-launcher"])
    }

    /// Most recently used installations first.
    fn status(&self) -> R<Status> {
        let Some(file) = current() else { return Ok(Status { protected: false, instances: vec![] }) };
        let (root, v) = (dir(), load(&file)?);
        let mut profiles: Vec<(&String, &Value)> = v["profiles"].as_object().map(|m| m.iter().collect()).unwrap_or_default();
        profiles.sort_by(|a, b| b.1["lastUsed"].as_str().cmp(&a.1["lastUsed"].as_str()));
        let instances = profiles
            .into_iter()
            .map(|(id, p)| {
                let name = match (p["name"].as_str().unwrap_or_default(), p["type"].as_str()) {
                    ("", Some("latest-snapshot")) => "Dernière snapshot".into(),
                    ("", _) => "Dernière version".into(),
                    (n, _) => n.to_string(),
                };
                let (game_version, loader) = version_of(&root, p["lastVersionId"].as_str().unwrap_or_default());
                let game_dir = game_dir(p, &root);
                let issue = if same_dir(&game_dir, &root) { Some(Issue::Shared) } else { (!ours(p)).then_some(Issue::Bypass) };
                Instance { key: id.clone(), name, game_version, loader, game_dir, issue }
            })
            .collect();
        Ok(Status { protected: backup().contains_key(&key(&file)), instances })
    }

    /// Every installation with its own folder; the shared ones stay as they are (see `separate`).
    fn protect(&self) -> R<Vec<String>> {
        closed()?;
        let (root, mut all) = (dir(), backup());
        let mut warn = vec![];
        for file in files() {
            let mut v = load(&file)?;
            let saved = all.entry(key(&file)).or_insert(json!({})).as_object_mut().ok_or("bad vanilla.json")?;
            for (id, p) in v["profiles"].as_object_mut().into_iter().flatten() {
                if same_dir(&game_dir(p, &root), &root) {
                    warn.push(format!("installation {} uses .minecraft itself: NOT protected until separated", p["name"]));
                } else {
                    wrap(id, p, saved)?;
                }
            }
            save(&file, &v)?;
        }
        std::fs::write(backup_path(), serde_json::to_vec_pretty(&all)?)?;
        Ok(warn)
    }

    fn restore(&self) -> R<()> {
        closed()?;
        let mut all = backup();
        for file in files() {
            let mut v = load(&file)?;
            let saved = all.remove(&key(&file)).unwrap_or_default();
            for (id, p) in v["profiles"].as_object_mut().into_iter().flatten() {
                if ours(p) {
                    match saved[id].as_str().filter(|j| !j.is_empty()) {
                        Some(j) => p["javaDir"] = json!(j),
                        None => drop(p.as_object_mut().and_then(|o| o.remove("javaDir"))),
                    }
                }
            }
            save(&file, &v)?;
        }
        std::fs::write(backup_path(), serde_json::to_vec_pretty(&all)?)?;
        Ok(())
    }
}

/// What a game dir holds that's the player's: copied, never moved, so other installations sharing
/// `.minecraft` keep theirs.
const PLAYER_FILES: [&str; 12] = [
    "mods", "config", "defaultconfigs", "saves", "resourcepacks", "shaderpacks", "schematics",
    "options.txt", "optionsof.txt", "optionsshaders.txt", "servers.dat", "servers.dat_old",
];

/// Give the shared installation `id` its own folder, `.minecraft\instances\<name>`, with a copy of
/// the player's files, and protect it if the launcher is protected. Returns the new folder.
// ponytail: copies saves too (worlds can be big); a world played after this lives in the new folder only.
pub fn separate(id: &str) -> R<PathBuf> {
    closed()?;
    let (root, file) = (dir(), current().ok_or("no launcher_profiles.json")?);
    let mut v = load(&file)?;
    let p = v["profiles"].get_mut(id).ok_or("no such installation")?;
    if !same_dir(&game_dir(p, &root), &root) {
        return Err("this installation already has its own folder".into());
    }
    let name: String = p["name"].as_str().filter(|n| !n.is_empty()).unwrap_or(id).chars().filter(|c| c.is_alphanumeric() || " -_.".contains(*c)).collect();
    let dest = root.join("instances").join(name.trim().trim_end_matches('.'));
    if dest.exists() {
        return Err(format!("{} already exists", dest.display()).into());
    }
    std::fs::create_dir_all(&dest)?;
    for f in PLAYER_FILES {
        copy(&root.join(f), &dest.join(f))?;
    }
    p["gameDir"] = json!(dest);
    let mut all = backup();
    if let Some(saved) = all.get_mut(&key(&file)).and_then(Value::as_object_mut) {
        wrap(id, p, saved)?;
        std::fs::write(backup_path(), serde_json::to_vec_pretty(&all)?)?;
    }
    save(&file, &v)?;
    Ok(dest)
}

fn copy(from: &Path, to: &Path) -> std::io::Result<()> {
    if from.is_dir() {
        std::fs::create_dir_all(to)?;
        for e in std::fs::read_dir(from)? {
            let e = e?;
            copy(&e.path(), &to.join(e.file_name()))?;
        }
    } else if from.exists() {
        std::fs::copy(from, to)?;
    }
    Ok(())
}

/// JAVA_HOME for a launch through the `auto` fake java: the Mojang runtime the game version asks
/// for (`javaVersion.component`, following `inheritsFrom` for loaders), which the launcher downloads.
pub fn runtime_for(args: &[String]) -> R<PathBuf> {
    let after = |flag: &str| args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1));
    let mut id = after("--version").ok_or("no --version in the java args")?.clone();
    // The launcher's root (`.minecraft`, or another --workDir) from the version jar on the classpath.
    let root = after("-cp")
        .into_iter()
        .flat_map(std::env::split_paths)
        .find_map(|e| {
            let versions = e.parent()?.parent()?;
            (versions.file_name()? == "versions").then(|| versions.parent().map(Path::to_path_buf)).flatten()
        })
        .unwrap_or_else(dir);
    let component = loop {
        let v = load(&root.join("versions").join(&id).join(format!("{id}.json")))?;
        if let Some(c) = v["javaVersion"]["component"].as_str() {
            break c.to_string();
        }
        match v["inheritsFrom"].as_str() {
            Some(p) if p != id => id = p.into(),
            _ => break "jre-legacy".into(), // versions older than javaVersion: Java 8
        }
    };
    let store = crate::env_path("LOCALAPPDATA").unwrap_or_default().join(r"Packages\Microsoft.4297127D64EC6_8wekyb3d8bbwe\LocalCache\Local");
    [root.join("runtime"), store.join("runtime")]
        .into_iter()
        .map(|r| r.join(&component).join(crate::sys::MOJANG_PLATFORM).join(&component))
        .chain([root.join("java").join(&component)]) // CurseForge: `Install\java\<component>`
        .find(|h| h.join("bin").join(crate::sys::JAVA).exists())
        .ok_or_else(|| format!("Mojang's java runtime {component} isn't downloaded: launch this version once without Guarden").into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn runtime_from_version_chain() {
        let t = std::env::temp_dir().join(format!("guarden-vanilla-{}", std::process::id()));
        let v = |id: &str, body: &str| {
            fs::create_dir_all(t.join("versions").join(id)).unwrap();
            fs::write(t.join("versions").join(id).join(format!("{id}.json")), body).unwrap();
        };
        v("1.20.1", r#"{"javaVersion":{"component":"java-runtime-gamma"}}"#);
        v("fabric-loader-0.16-1.20.1", r#"{"inheritsFrom":"1.20.1"}"#);
        let home = t.join("runtime").join("java-runtime-gamma").join(crate::sys::MOJANG_PLATFORM).join("java-runtime-gamma");
        fs::create_dir_all(home.join("bin")).unwrap();
        fs::write(home.join("bin").join(crate::sys::JAVA), "").unwrap();
        let jar = t.join("versions").join("fabric-loader-0.16-1.20.1").join("fabric-loader-0.16-1.20.1.jar");
        let cp = std::env::join_paths([Path::new("/lib/a.jar"), &jar]).unwrap().into_string().unwrap();
        let args: Vec<String> = ["-cp", &cp, "M", "--version", "fabric-loader-0.16-1.20.1"].map(Into::into).into();
        assert_eq!(runtime_for(&args).unwrap(), home);
        assert_eq!(version_of(&t, "fabric-loader-0.16-1.20.1"), (Some("1.20.1".into()), Some("fabric".into())));
        assert_eq!(version_of(&t, "1.21.4"), (Some("1.21.4".into()), None));
        assert!(same_dir(Path::new("C:/Users/x/.minecraft/"), Path::new(r"c:\users\x\.minecraft")));
        fs::remove_dir_all(&t).unwrap();
    }
}
