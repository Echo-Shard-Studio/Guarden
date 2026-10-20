//! Windows: the sandbox is an AppContainer per instance (see `sandbox`). The rest of the crate only
//! goes through what this module exposes (`crate::sys`); another OS provides the same functions.
mod acl;
mod sandbox;

pub use sandbox::{allow_listing, has_localhost, prepare, run, run_elevated};

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};

/// Exe names of a java (fake javas get these).
pub const JAVAS: [&str; 2] = ["java.exe", "javaw.exe"];
/// The one launchers are pointed at: no console window.
pub const JAVA: &str = "javaw.exe";

pub fn home() -> PathBuf {
    crate::env_path("USERPROFILE").unwrap_or_default()
}

/// `%APPDATA%`: where launchers (and Guarden) keep their data.
pub fn data_dir() -> PathBuf {
    crate::env_path("APPDATA").unwrap_or_default()
}

/// Where Electron apps keep their settings: `%APPDATA%` too.
pub fn config_dir() -> PathBuf {
    data_dir()
}

/// Never exposed to the game, nor any dir above them (see `deny_list`).
pub fn private_dirs() -> Vec<PathBuf> {
    ["USERPROFILE", "APPDATA", "LOCALAPPDATA"].into_iter().filter_map(crate::env_path).collect()
}

const CREATE_NO_WINDOW: u32 = 0x0800_0000; // called from the GUI: no console flash

/// Is any of these processes running? (exe names, case-insensitive)
// ponytail: process name check; a renamed launcher exe slips through.
pub fn running(names: &[&str]) -> bool {
    let Ok(out) = std::process::Command::new("tasklist").args(["/FO", "CSV", "/NH"]).creation_flags(CREATE_NO_WINDOW).output() else {
        return false;
    };
    let list = String::from_utf8_lossy(&out.stdout).to_lowercase();
    names.iter().any(|n| list.contains(&format!("\"{}\"", n.to_lowercase())))
}

/// Show `dir` in the file manager.
pub fn open(dir: &Path) -> std::io::Result<()> {
    std::process::Command::new("explorer").arg(dir).spawn().map(drop)
}

/// Append `a` to a Windows command line, quoted the way CommandLineToArgvW / the MSVC CRT parse it.
fn push_arg(cmd: &mut String, a: &str) {
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

#[cfg(test)]
mod tests {
    #[test]
    fn quoting() {
        let q = |a: &str| {
            let mut s = String::new();
            super::push_arg(&mut s, a);
            s
        };
        assert_eq!(q(r"C:\x\y.jar"), r"C:\x\y.jar");
        assert_eq!(q("a b"), r#""a b""#);
        assert_eq!(q(r"C:\x y\"), r#""C:\x y\\""#);
        assert_eq!(q(r#"a"b"#), r#""a\"b""#);
        assert_eq!(q(r#"a\"b"#), r#""a\\\"b""#);
        assert_eq!(q(""), r#""""#);
    }
}
