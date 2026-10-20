//! Prism Launcher: protect = point its wrapper command at the shim, restore = clear it.
//! Settings: `<data dir>/PrismLauncher/prismlauncher.cfg` (also as a Flatpak), per instance
//! `instances/<x>/instance.cfg`,
//! both QSettings INI. Prism hands the game's args to java over stdin (see the shim), not argv.
use crate::{Instance, Issue, R, Status};
use std::path::{Path, PathBuf};

pub struct Prism;

pub fn dir() -> PathBuf {
    data().join("PrismLauncher")
}

fn data() -> PathBuf {
    crate::launcher_data("PrismLauncher", "org.prismlauncher.PrismLauncher")
}

fn cfg() -> PathBuf {
    dir().join("prismlauncher.cfg")
}

/// The wrapper command as Prism wants it: it splits on spaces (quotes group), and depending on its
/// version a `\` may escape the next char, so forward slashes, which Windows accepts too.
fn hook() -> String {
    format!("\"{}\"", crate::shim_in(&data()).to_string_lossy().replace('\\', "/"))
}

/// QSettings INI value of `key` in `[General]` (or before any section), unescaped.
fn get(ini: &str, key: &str) -> Option<String> {
    let mut general = true;
    for line in ini.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            general = line == "[General]";
        } else if general && let Some((k, v)) = line.split_once('=') && k.trim() == key {
            return Some(unescape(v.trim()));
        }
    }
    None
}

/// QSettings' string escaping, the parts settings use: `"..."` groups, `\\` `\"` `\n` `\t` `\xHHHH`.
// ponytail: an unquoted `a, b` is a string list to QSettings; read here as the plain text.
fn unescape(v: &str) -> String {
    let mut out = String::new();
    let mut chars = v.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {}
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('x') => {
                    let hex: String = chars.by_ref().take(4).collect();
                    out.extend(u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32));
                }
                Some(c) => out.push(c),
                None => {}
            },
            c => out.push(c),
        }
    }
    out
}

/// `ini` with `key` in `[General]` set to `value` (quoted, escaped), or removed for None.
fn set(ini: &str, key: &str, value: Option<&str>) -> String {
    let nl = if ini.contains("\r\n") { "\r\n" } else { "\n" };
    let line = value.map(|v| format!("{key}=\"{}\"", v.replace('\\', r"\\").replace('"', r#"\""#)));
    let mut out: Vec<String> = vec![];
    let (mut general, mut done) = (true, false);
    for l in ini.lines() {
        let t = l.trim();
        if t.starts_with('[') {
            general = t == "[General]";
        } else if general && t.split_once('=').is_some_and(|(k, _)| k.trim() == key) {
            out.extend(line.clone());
            done = true;
            continue;
        }
        out.push(l.into());
    }
    if let (false, Some(line)) = (done, line) {
        match out.iter().position(|l| l.trim() == "[General]") {
            Some(i) => out.insert(i + 1, line),
            None => {
                out.insert(0, line);
                out.insert(0, "[General]".into());
            },
        }
    }
    out.join(nl) + nl
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_default()
}

/// The instances dir (`InstanceDir`, relative to the data dir) and each `(folder, instance.cfg text)`.
fn instances() -> Vec<(PathBuf, String)> {
    let base = dir().join(get(&read(&cfg()), "InstanceDir").filter(|d| !d.is_empty()).unwrap_or("instances".into()));
    let mut v: Vec<(PathBuf, String)> = std::fs::read_dir(base)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter_map(|p| std::fs::read_to_string(p.join("instance.cfg")).ok().map(|c| (p, c)))
        .collect();
    v.sort();
    v
}

/// An instance's own commands replace the global ones, so its wrapper must be ours too.
fn overrides(inst: &str) -> bool {
    get(inst, "OverrideCommands").is_some_and(|v| v == "true")
}

/// Same rule as Prism's `MinecraftInstance::gameRoot`.
fn game_root(inst: &Path) -> PathBuf {
    let (mc, dot) = (inst.join("minecraft"), inst.join(".minecraft"));
    if dot.exists() && !mc.exists() { dot } else { mc }
}

/// Minecraft version and loader from `mmc-pack.json`.
fn pack(inst: &Path) -> (Option<String>, Option<String>) {
    let json: serde_json::Value = std::fs::read(inst.join("mmc-pack.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let comps = json["components"].as_array().cloned().unwrap_or_default();
    let version = comps.iter().find(|c| c["uid"] == "net.minecraft").and_then(|c| c["version"].as_str()).map(Into::into);
    let loader = comps.iter().find_map(|c| match c["uid"].as_str()? {
        "net.fabricmc.fabric-loader" => Some("fabric"),
        "org.quiltmc.quilt-loader" => Some("quilt"),
        "net.minecraftforge" => Some("forge"),
        "net.neoforged" => Some("neoforge"),
        _ => None,
    });
    (version, loader.map(Into::into))
}

fn closed() -> R<()> {
    if crate::Launcher::is_running(&Prism) {
        return Err("close Prism Launcher first, it would overwrite its settings".into());
    }
    Ok(())
}

impl crate::Launcher for Prism {
    fn id(&self) -> &'static str {
        "prism"
    }
    fn name(&self) -> &'static str {
        "Prism Launcher"
    }
    // ponytail: the default data dir only; portable installs (portable.txt next to the exe) aren't found.
    fn installed(&self) -> bool {
        cfg().exists()
    }
    fn is_running(&self) -> bool {
        crate::sys::running(&["prismlauncher.exe"])
    }

    fn status(&self) -> R<Status> {
        let protected = get(&read(&cfg()), "WrapperCommand").is_some_and(|w| w == hook());
        let instances = instances()
            .into_iter()
            .map(|(p, c)| {
                let key = p.file_name().unwrap_or_default().to_string_lossy().into_owned();
                let (game_version, loader) = pack(&p);
                let issue = (overrides(&c) && get(&c, "WrapperCommand").is_none_or(|w| w != hook())).then_some(Issue::Bypass);
                Instance { name: get(&c, "name").unwrap_or(key.clone()), key, game_version, loader, game_dir: game_root(&p), issue }
            })
            .collect();
        Ok(Status { protected, instances })
    }

    fn protect(&self) -> R<Vec<String>> {
        closed()?;
        let global = read(&cfg());
        if get(&global, "ConfigVersion").is_none() {
            return Err("this Prism Launcher is too old, update it first".into());
        }
        if let Some(w) = get(&global, "WrapperCommand").filter(|w| !w.is_empty() && *w != hook()) {
            return Err(format!("Prism already has a wrapper command ({w}), Guarden won't replace it").into());
        }
        crate::shim_for(&data())?;
        crate::write_settings(&cfg(), set(&global, "WrapperCommand", Some(&hook())).as_bytes())?;
        let mut warn = vec![];
        for (p, c) in instances().into_iter().filter(|(_, c)| overrides(c)) {
            match get(&c, "WrapperCommand").filter(|w| !w.is_empty()) {
                None => crate::write_atomic(&p.join("instance.cfg"), set(&c, "WrapperCommand", Some(&hook())).as_bytes())?,
                Some(w) if w != hook() => warn.push(format!("instance {} has its own wrapper command: NOT protected", p.display())),
                Some(_) => {}
            }
        }
        Ok(warn)
    }

    // ponytail: ACEs and container profiles stay, as for Modrinth.
    fn restore(&self) -> R<()> {
        closed()?;
        let global = read(&cfg());
        if get(&global, "WrapperCommand").is_some_and(|w| w == hook()) {
            crate::write_settings(&cfg(), set(&global, "WrapperCommand", None).as_bytes())?;
        }
        for (p, c) in instances() {
            if get(&c, "WrapperCommand").is_some_and(|w| w == hook()) {
                crate::write_atomic(&p.join("instance.cfg"), set(&c, "WrapperCommand", None).as_bytes())?;
            }
        }
        Ok(())
    }
}

/// Prism's launch: java gets `org.prismlauncher.EntryPoint` and reads a script on stdin up to a
/// `launch` (or `abort`) line; `param <arg>` lines are the game args (`--gameDir` ...). Reads that
/// script from `input`, returns it raw (to pass on to java) and its params.
pub fn read_script(input: &mut impl std::io::BufRead) -> std::io::Result<(Vec<u8>, Vec<String>)> {
    let (mut raw, mut params) = (vec![], vec![]);
    loop {
        let start = raw.len();
        if input.read_until(b'\n', &mut raw)? == 0 {
            break;
        }
        let line = String::from_utf8_lossy(&raw[start..]);
        let line = line.trim_end_matches(['\r', '\n']);
        if let Some(p) = line.strip_prefix("param ") {
            params.push(p.into());
        }
        if line == "launch" || line == "abort" {
            break;
        }
    }
    Ok((raw, params))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ini_roundtrip() {
        let ini = "[General]\r\nConfigVersion=1.3\r\nJavaPath=C:/Program Files/Java/bin/javaw.exe\r\nname=\"a, b\"\r\n[Other]\r\nWrapperCommand=x\r\n";
        assert_eq!(get(ini, "JavaPath").as_deref(), Some("C:/Program Files/Java/bin/javaw.exe"));
        assert_eq!(get(ini, "name").as_deref(), Some("a, b"));
        assert_eq!(get(ini, "WrapperCommand"), None); // other section
        assert_eq!(get(r#"P="C:\\x \"y\"\x00e9""#, "P").as_deref(), Some(r#"C:\x "y"é"#));

        let hook = r#""C:/Users/Jo Doe/guarden-shim.exe""#;
        let on = set(ini, "WrapperCommand", Some(hook));
        assert!(on.contains("[General]\r\nWrapperCommand=\"\\\"C:/Users/Jo Doe/guarden-shim.exe\\\"\"\r\n"));
        assert_eq!(get(&on, "WrapperCommand").as_deref(), Some(hook));
        assert_eq!(set(&on, "WrapperCommand", None), ini);
        assert_eq!(set("", "K", Some("v")), "[General]\nK=\"v\"\n");
    }

    #[test]
    fn script() {
        let mut input = &b"mainClass M\nparam --gameDir\nparam C:/i/minecraft\nlauncher standard\nlaunch\nrest"[..];
        let (raw, params) = read_script(&mut input).unwrap();
        assert_eq!(params, ["--gameDir", "C:/i/minecraft"]);
        assert!(raw.ends_with(b"launch\n"));
        assert_eq!(input, b"rest");
    }
}
