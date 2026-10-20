//! CurseForge: protect = point each of its java paths at a fake java (see `fake_java`), restore = put
//! the originals back (kept in `<Guarden root>\curseforge.json`). Instances only pick among those paths.
//! Settings: `%APPDATA%\CurseForge\storage.json` (`~/.config/CurseForge` on Linux), whose values are
//! JSON strings; `minecraft-settings` holds `javaVersionsPaths: {"java8": "", "java25": "C:\\...\\javaw.exe"}`,
//! empty = CurseForge's own java in `<minecraftRoot>\Install\java\<component>`.
use crate::{Instance, Issue, R, Status};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub struct CurseForge;

/// Its settings (an Electron app: `%APPDATA%`, `~/.config`).
pub fn dir() -> PathBuf {
    crate::sys::config_dir().join("CurseForge")
}

fn storage() -> PathBuf {
    dir().join("storage.json")
}

/// Where CurseForge keeps instances (`Instances`) and the shared game files (`Install`), by default.
pub fn default_root() -> PathBuf {
    crate::sys::home().join("curseforge").join("minecraft")
}

/// storage.json, and its decoded `minecraft-settings`.
fn load() -> R<(Value, Value)> {
    let top: Value = serde_json::from_slice(&std::fs::read(storage())?)?;
    let settings = serde_json::from_str(top["minecraft-settings"].as_str().unwrap_or("{}"))?;
    Ok((top, settings))
}

fn save(mut top: Value, settings: &Value) -> R<()> {
    top["minecraft-settings"] = json!(serde_json::to_string(settings)?);
    Ok(crate::write_settings(&storage(), serde_json::to_string_pretty(&top)?.as_bytes())?)
}

/// Its Minecraft folder as set, or the default.
pub fn minecraft_root() -> PathBuf {
    load().map_or_else(|_| default_root(), |(_, s)| root(&s))
}

fn root(settings: &Value) -> PathBuf {
    settings["minecraftRoot"].as_str().filter(|r| !r.is_empty()).map_or_else(default_root, PathBuf::from)
}

/// `{"java25": "<original path, or empty>"}`. Present = protected.
fn backup_path() -> PathBuf {
    crate::root().join("curseforge.json")
}

/// Java major version → CurseForge's own JAVA_HOME for it, from the versions it has installed.
fn runtimes(install: &Path) -> BTreeMap<u64, PathBuf> {
    let mut out = BTreeMap::new();
    for d in std::fs::read_dir(install.join("versions")).into_iter().flatten().flatten() {
        let json = d.path().join(format!("{}.json", d.file_name().to_string_lossy()));
        let Some(v) = std::fs::read(json).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()) else { continue };
        if let (Some(major), Some(c)) = (v["javaVersion"]["majorVersion"].as_u64(), v["javaVersion"]["component"].as_str()) {
            let home = install.join("java").join(c);
            if home.join("bin").join(crate::sys::JAVA).exists() {
                out.insert(major, home);
            }
        }
    }
    out
}

/// The java major a game version asks for, if CurseForge has installed it.
fn major_of(install: &Path, game_version: &str) -> Option<u64> {
    let json = install.join("versions").join(game_version).join(format!("{game_version}.json"));
    serde_json::from_slice::<Value>(&std::fs::read(json).ok()?).ok()?["javaVersion"]["majorVersion"].as_u64()
}

fn is_ours(path: Option<&str>) -> bool {
    path.is_some_and(|p| crate::is_fake_java(Path::new(p)))
}

fn closed() -> R<()> {
    if crate::Launcher::is_running(&CurseForge) {
        return Err("quit CurseForge first (also from the tray icon), it would overwrite its settings".into());
    }
    Ok(())
}

impl crate::Launcher for CurseForge {
    fn id(&self) -> &'static str {
        "curseforge"
    }
    fn name(&self) -> &'static str {
        "CurseForge"
    }
    fn installed(&self) -> bool {
        storage().exists()
    }
    fn is_running(&self) -> bool {
        crate::sys::running(&["CurseForge.exe"])
    }

    /// Most recently played first.
    fn status(&self) -> R<Status> {
        let (_, settings) = load()?;
        let (root, paths) = (root(&settings), &settings["javaVersionsPaths"]);
        let install = root.join("Install");
        let mut found: Vec<(String, Instance)> = std::fs::read_dir(root.join("Instances"))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|d| {
                let j: Value = serde_json::from_slice(&std::fs::read(d.path().join("minecraftinstance.json")).ok()?).ok()?;
                let key = d.file_name().to_string_lossy().into_owned();
                let game_version = j["gameVersion"].as_str().map(String::from);
                let l = j["baseModLoader"]["name"].as_str().unwrap_or_default().to_lowercase();
                let loader = ["fabric", "quilt", "neoforge", "forge"].into_iter().find(|n| l.starts_with(n)).map(String::from);
                // Its java isn't ours: a java CurseForge didn't have yet when protecting.
                let major = game_version.as_deref().and_then(|v| major_of(&install, v));
                let issue = major.filter(|m| !is_ours(paths[format!("java{m}")].as_str())).map(|_| Issue::Bypass);
                let game_dir = j["installPath"].as_str().filter(|p| !p.is_empty()).map_or(d.path(), PathBuf::from);
                let played = j["lastPlayed"].as_str().unwrap_or_default().to_string();
                Some((played, Instance { name: j["name"].as_str().map_or(key.clone(), String::from), key, game_version, loader, game_dir, issue }))
            })
            .collect();
        found.sort_by(|a, b| b.0.cmp(&a.0));
        Ok(Status { protected: backup_path().exists(), instances: found.into_iter().map(|(_, i)| i).collect() })
    }

    /// Every java path: a custom one gets wrapped, an empty one gets CurseForge's own java, wrapped.
    fn protect(&self) -> R<Vec<String>> {
        closed()?;
        let (top, mut settings) = load()?;
        let install = root(&settings).join("Install");
        let mut saved: Map<String, Value> = std::fs::read(backup_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let mut warn = vec![];
        if settings["gameLaunchMethod"].as_u64() != Some(1) {
            // ponytail: only CurseForge's own launch is known; through the Mojang launcher, unverified.
            warn.push("CurseForge doesn't launch the game itself (Settings > Minecraft > launcher): Guarden may be skipped".into());
        }
        if !settings["javaVersionsPaths"].is_object() {
            settings["javaVersionsPaths"] = json!({});
        }
        let paths = settings["javaVersionsPaths"].as_object_mut().ok_or("bad javaVersionsPaths")?;
        let runtimes = runtimes(&install);
        let mut keys: Vec<String> = paths.keys().cloned().collect();
        keys.extend(runtimes.keys().map(|m| format!("java{m}")));
        keys.sort();
        keys.dedup();
        for k in keys {
            let cur = paths.get(&k).and_then(Value::as_str).unwrap_or_default().to_string();
            if is_ours(Some(&cur)) {
                continue;
            }
            let home = if cur.is_empty() {
                let major = k.strip_prefix("java").and_then(|m| m.parse().ok());
                match major.and_then(|m| runtimes.get(&m)) {
                    Some(h) => h.clone(),
                    None => {
                        warn.push(format!("{k}: CurseForge hasn't downloaded it yet, protect again after a first launch"));
                        continue;
                    }
                }
            } else {
                Path::new(&cur).parent().and_then(Path::parent).ok_or("bad java path")?.to_path_buf()
            };
            paths.insert(k.clone(), json!(crate::fake_java(Some(&home))?));
            saved.insert(k, json!(cur));
        }
        save(top, &settings)?;
        std::fs::write(backup_path(), serde_json::to_vec_pretty(&saved)?)?;
        Ok(warn)
    }

    fn restore(&self) -> R<()> {
        closed()?;
        let (top, mut settings) = load()?;
        let saved: Map<String, Value> = std::fs::read(backup_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        if let Some(paths) = settings["javaVersionsPaths"].as_object_mut() {
            for (k, v) in paths.iter_mut().filter(|(_, v)| is_ours(v.as_str())) {
                *v = saved.get(k).cloned().unwrap_or(json!(""));
            }
        }
        save(top, &settings)?;
        let _ = std::fs::remove_file(backup_path());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn runtimes_by_major() {
        let t = std::env::temp_dir().join(format!("guarden-cf-{}", std::process::id()));
        let v = |id: &str, body: &str| {
            fs::create_dir_all(t.join("versions").join(id)).unwrap();
            fs::write(t.join("versions").join(id).join(format!("{id}.json")), body).unwrap();
        };
        v("26.3", r#"{"javaVersion":{"component":"java-runtime-epsilon","majorVersion":25}}"#);
        v("1.20.1", r#"{"javaVersion":{"component":"java-runtime-gamma","majorVersion":17}}"#); // not downloaded
        v("fabric-loader-0.19.5-26.3", r#"{"inheritsFrom":"26.3"}"#);
        let home = t.join("java").join("java-runtime-epsilon");
        fs::create_dir_all(home.join("bin")).unwrap();
        fs::write(home.join("bin").join(crate::sys::JAVA), "").unwrap();
        assert_eq!(runtimes(&t), BTreeMap::from([(25, home)]));
        assert_eq!(major_of(&t, "1.20.1"), Some(17));
        assert_eq!(major_of(&t, "1.8.9"), None);
        fs::remove_dir_all(&t).unwrap();
    }
}
