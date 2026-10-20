pub mod policy;
#[cfg(windows)]
pub mod acl;
#[cfg(windows)]
pub mod sandbox;
#[cfg(windows)]
pub mod modrinth;

use std::path::PathBuf;

fn env_path(var: &str) -> Option<PathBuf> {
    std::env::var_os(var).filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// `%APPDATA%\Guarden`: shims, real.txt files, logs. The container never gets access here.
pub fn root() -> PathBuf {
    env_path("APPDATA").unwrap_or_default().join("Guarden")
}

/// The shim launchers run, under the Guarden root (no container can write there). A copy of the one
/// shipped next to the app, so a launcher never points into the install dir that updates replace.
pub fn installed_shim() -> PathBuf {
    root().join("guarden-shim.exe")
}

/// After an app update: replace the installed shim if it differs from `shipped`. Returns whether it
/// copied. Fails while a game runs (the shim is in use); the next start retries.
pub fn refresh_shim(shipped: &std::path::Path) -> std::io::Result<bool> {
    let dst = installed_shim();
    if !dst.exists() || std::fs::read(&dst)? == std::fs::read(shipped)? {
        return Ok(false);
    }
    std::fs::copy(shipped, &dst).map(|_| true)
}

/// Dirs that must never be exposed to the game, directly or through a grant on an ancestor.
// ponytail: Modrinth only for now; add each launcher root (where its tokens live) as it gets supported.
// The official launcher's .minecraft holds tokens AND is the default gameDir, needs its own answer.
pub fn deny_list() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = ["USERPROFILE", "APPDATA", "LOCALAPPDATA"].into_iter().filter_map(env_path).collect();
    if let Some(a) = env_path("APPDATA") {
        v.push(a.join("ModrinthApp"));
    }
    v.push(root());
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
pub fn container_name(game_dir: &std::path::Path) -> String {
    format!("Guarden.{}", short_hash(&game_dir.to_string_lossy()))
}

/// Append `a` to a Windows command line, quoted the way CommandLineToArgvW / the MSVC CRT parse it.
pub fn push_arg(cmd: &mut String, a: &str) {
    if !cmd.is_empty() {
        cmd.push(' ');
    }
    if !a.is_empty() && !a.contains([' ', '\t', '\n', '\x0b', '"']) {
        cmd.push_str(a);
        return;
    }
    cmd.push('"');
    let mut bs = 0;
    for c in a.chars() {
        match c {
            '\\' => bs += 1,
            '"' => {
                cmd.extend(std::iter::repeat_n('\\', bs * 2 + 1));
                cmd.push('"');
                bs = 0;
            }
            _ => {
                cmd.extend(std::iter::repeat_n('\\', bs));
                cmd.push(c);
                bs = 0;
            }
        }
    }
    cmd.extend(std::iter::repeat_n('\\', bs * 2));
    cmd.push('"');
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
    fn quoting() {
        let q = |a: &str| {
            let mut s = String::new();
            push_arg(&mut s, a);
            s
        };
        assert_eq!(q(r"C:\x\y.jar"), r"C:\x\y.jar");
        assert_eq!(q("a b"), r#""a b""#);
        assert_eq!(q(r"C:\x y\"), r#""C:\x y\\""#);
        assert_eq!(q(r#"a"b"#), r#""a\"b""#);
        assert_eq!(q(r#"a\"b"#), r#""a\\\"b""#);
        assert_eq!(q(""), r#""""#);
    }

    #[test]
    fn hash_is_stable_and_case_insensitive() {
        assert_eq!(short_hash(r"C:\Game"), short_hash(r"c:\game"));
        assert_eq!(short_hash(""), "cbf29ce484222325");
    }
}
