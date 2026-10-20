//! Guarden CLI.
//! Used by the app (rerun elevated) and for testing by hand.
//! - `guarden protect modrinth` / `guarden restore modrinth`: switch Modrinth's wrapper hook (admin shell).
//! - `guarden prepare modrinth`: (re)create the instances' containers, e.g. for a new instance (admin).
//! - `guarden setup`: one-time admin part of the install, run by the installer.
//! - `guarden install <JAVA_HOME>`: fake java for launchers without a wrapper hook; prints the path
//!   to paste into the launcher's Java setting.
use std::{env, fs, path::PathBuf};

type R = Result<(), Box<dyn std::error::Error>>;

fn main() -> R {
    let args: Vec<String> = env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["install", home] => install(home),
        #[cfg(windows)]
        ["protect", "modrinth"] => {
            let modrinth = PathBuf::from(env::var_os("APPDATA").unwrap_or_default()).join("ModrinthApp");
            allow_listing(&[modrinth.join("meta"), modrinth.join("profiles")]);
            for w in guarden::modrinth::protect(&shim()?)? {
                eprintln!("warning: {w}");
            }
            println!("Modrinth protected");
            Ok(())
        }
        // Run by the installer (admin). Containers must list every ancestor of their grants, and
        // only admin can grant that on C:\ and C:\Users; the shim does the user-owned rest per launch.
        #[cfg(windows)]
        ["setup"] => {
            allow_listing(&[PathBuf::from(env::var_os("USERPROFILE").unwrap_or_default())]);
            Ok(())
        }
        #[cfg(windows)]
        ["prepare", "modrinth"] => {
            for w in guarden::modrinth::prepare()? {
                eprintln!("warning: {w}");
            }
            Ok(())
        }
        #[cfg(windows)]
        ["restore", "modrinth"] => {
            guarden::modrinth::restore()?;
            println!("Modrinth restored");
            Ok(())
        }
        _ => {
            eprintln!("usage: guarden setup | protect|prepare|restore modrinth | install <JAVA_HOME>");
            std::process::exit(2)
        }
    }
}

fn shim() -> Result<PathBuf, std::io::Error> {
    Ok(env::current_exe()?.with_file_name("guarden-shim.exe"))
}

/// Containers must list every ancestor of what they're granted (see `Policy::ancestors`); `C:\` and
/// `C:\Users` need admin, so do them here, from an admin shell. The shim does the rest per launch.
#[cfg(windows)]
fn allow_listing(grants: &[PathBuf]) {
    if let Err(e) = guarden::sandbox::allow_listing(&guarden::policy::Policy { rw: vec![], ro: grants.to_vec() }.ancestors()) {
        eprintln!("warning: {e}");
    }
}

fn install(home: &str) -> R {
    // components(): drops a trailing `\` (tab-completion adds one), which would change the id.
    let home: PathBuf = std::path::absolute(home)?.components().collect();
    if !home.join("bin").join("javaw.exe").exists() {
        return Err(format!("{} is not a JAVA_HOME (no bin\\javaw.exe)", home.display()).into());
    }
    let dir = guarden::root().join("java").join(guarden::short_hash(&home.to_string_lossy()));
    fs::create_dir_all(dir.join("bin"))?;
    for name in ["java.exe", "javaw.exe"] {
        fs::copy(shim()?, dir.join("bin").join(name))?;
    }
    fs::write(dir.join("real.txt"), home.to_string_lossy().as_bytes())?;
    #[cfg(windows)]
    allow_listing(std::slice::from_ref(&home));
    println!("{}", dir.join("bin").join("javaw.exe").display());
    Ok(())
}
