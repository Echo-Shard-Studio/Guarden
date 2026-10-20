//! Sandboxing java launcher, used two ways: as the launcher's wrapper hook (`guarden-shim <java> <args>`),
//! or as a fake `java.exe` / `javaw.exe` in `%APPDATA%\Guarden\java\<id>\bin\` with `<id>\real.txt` holding
//! the real JAVA_HOME (or `auto`). Either way it relaunches the real java inside an AppContainer.
#![windows_subsystem = "windows"]

fn main() {
    std::panic::set_hook(Box::new(|info| log(&format!("panic: {info}")))); // GUI subsystem: panics are otherwise invisible
    std::process::exit(match run() {
        Ok((container, code)) => {
            log(&format!("{container} exited with {code:#x}")); // read back by guarden::launches()
            code as i32
        }
        Err(e) => {
            let msg = format!("guarden: {e}");
            eprintln!("{msg}"); // the launcher shows the game's stderr
            log(&msg);
            1
        }
    })
}

fn run() -> Result<(String, u32), Box<dyn std::error::Error>> {
    use guarden::{policy, sys};
    use std::path::{Path, PathBuf};

    let me = std::env::current_exe()?;
    let mut args = std::env::args_os()
        .skip(1)
        .map(|a| a.into_string().map_err(|_| "non-unicode argument"))
        .collect::<Result<Vec<_>, _>>()?;
    let real_txt = me.parent().and_then(Path::parent).map(|d| d.join("real.txt"));
    let (exe, home) = match real_txt.and_then(|p| std::fs::read_to_string(p).ok()) {
        // Fake java: `<id>\bin\java(w).exe`, the real JAVA_HOME is in `<id>\real.txt`,
        // or `auto`: the Mojang runtime of the version being launched (official launcher).
        Some(home) => {
            let home = if home.trim() == "auto" { guarden::vanilla::runtime_for(&args)? } else { PathBuf::from(home.trim()) };
            (home.join("bin").join(me.file_name().ok_or("no exe name")?), home)
        }
        // Launcher wrapper hook (Modrinth): `guarden-shim <java> <args...>`.
        None => {
            let exe = PathBuf::from(if args.is_empty() { return Err("usage: guarden-shim <java> <args...>".into()) } else { args.remove(0) });
            let name = exe.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
            if !sys::JAVAS.contains(&name.as_str()) {
                return Err(format!("{} is not {}", exe.display(), sys::JAVAS.join("/")).into());
            }
            let home = exe.parent().and_then(Path::parent).ok_or("java is not in <home>\\bin\\")?.to_path_buf();
            (exe, home)
        }
    };

    // Prism sends the game args (--gameDir...) on stdin, not argv: read them before granting, then
    // hand java the script as it came.
    let (input, mut seen) = if args.iter().any(|a| a == "org.prismlauncher.EntryPoint") {
        let (raw, params) = guarden::prism::read_script(&mut std::io::stdin().lock())?;
        (Some(raw), [args.clone(), params].concat())
    } else {
        (None, args.clone())
    };
    if let Ok(cp) = std::env::var("CLASSPATH") {
        seen.extend(["-cp".into(), cp]); // java reads it when there's no -cp (Prism on Linux)
    }
    let pol = policy::from_args(&seen, &home, &guarden::deny_list())?;
    // No gameDir = launcher probing java.
    let container = pol.rw.first().map_or("Guarden.probe".into(), |d| guarden::container_name(d));
    log(&format!("{container} {} rw={:?} ro={:?}", exe.display(), pol.rw, pol.ro));

    // Debug: create `%APPDATA%\Guarden\debug` to get the game's args and output in logs\.
    // Game launches only: the launcher's java probes must keep their stdout or it can't read the version.
    let output = if !pol.rw.is_empty() && guarden::root().join("debug").exists() {
        let mut shown = args.clone();
        if let Some(t) = shown.iter().position(|a| a == "--accessToken").and_then(|i| shown.get_mut(i + 1)) {
            *t = "***".into();
        }
        log(&format!("args: {shown:?}"));
        Some(std::fs::OpenOptions::new().create(true).append(true).open(guarden::root().join("logs").join("java.log"))?)
    } else {
        None
    };
    let code = sys::run(&container, &exe, &args, &pol, output.as_ref(), input)?;
    Ok((container, code))
}

fn log(msg: &str) {
    use std::io::Write;
    let dir = guarden::root().join("logs");
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("shim.log")) {
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
        let _ = writeln!(f, "[{t}] {msg}");
    }
}
