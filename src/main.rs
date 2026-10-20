//! Guarden CLI.
//! Used by the app (rerun elevated) and for testing by hand. `<launcher>`: modrinth, prism, curseforge, official.
//! - `guarden protect <launcher>` / `guarden restore <launcher>`: switch the launcher (admin shell for
//!   Modrinth, whose containers need their localhost exemption).
//! - `guarden prepare <launcher>`: (re)create the instances' containers, e.g. for a new instance (admin).
//! - `guarden setup`: one-time admin part of the install, run by the installer.
//! - `guarden install <JAVA_HOME>`: fake java for launchers without a wrapper hook; prints the path
//!   to paste into the launcher's Java setting.
use std::{env, path::PathBuf};

type R = Result<(), Box<dyn std::error::Error>>;

fn main() -> R {
    let args: Vec<String> = env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["install", home] => install(home),
        // Run by the installer (admin). Containers must list every ancestor of their grants, and
        // only admin can grant that on C:\ and C:\Users; the shim does the user-owned rest per launch.
        ["setup"] => {
            allow_listing(&[guarden::sys::home()]);
            Ok(())
        }
        [cmd @ ("protect" | "prepare" | "restore"), id] => {
            let l = guarden::launcher(id).ok_or_else(|| format!("unknown launcher {id}"))?;
            let warn = match *cmd {
                "protect" => {
                    guarden::update_shims(&shim()?, true)?;
                    l.protect()?
                }
                "prepare" => guarden::prepare(l)?,
                _ => l.restore().map(|_| vec![])?,
            };
            for w in warn {
                eprintln!("warning: {w}");
            }
            println!("{} {cmd}: done", l.name());
            Ok(())
        }
        _ => {
            eprintln!("usage: guarden setup | protect|prepare|restore modrinth|prism|curseforge|official | install <JAVA_HOME>");
            std::process::exit(2)
        }
    }
}

fn shim() -> Result<PathBuf, std::io::Error> {
    Ok(env::current_exe()?.with_file_name(format!("guarden-shim{}", env::consts::EXE_SUFFIX)))
}

/// Containers must list every ancestor of what they're granted (see `Policy::ancestors`); `C:\` and
/// `C:\Users` need admin, so do them here, from an admin shell. The shim does the rest per launch.
fn allow_listing(grants: &[PathBuf]) {
    if let Err(e) = guarden::sys::allow_listing(&guarden::policy::Policy { rw: vec![], ro: grants.to_vec() }.ancestors()) {
        eprintln!("warning: {e}");
    }
}

fn install(home: &str) -> R {
    let home = std::path::absolute(home)?;
    if !home.join("bin").join(guarden::sys::JAVA).exists() {
        return Err(format!("{} is not a JAVA_HOME (no bin/{})", home.display(), guarden::sys::JAVA).into());
    }
    guarden::update_shims(&shim()?, true)?;
    let java = guarden::fake_java(Some(&home))?;
    allow_listing(std::slice::from_ref(&home));
    println!("{}", java.display());
    Ok(())
}
