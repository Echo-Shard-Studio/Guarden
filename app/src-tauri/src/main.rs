//! Guarden desktop app. Shows the launchers and their instances, and switches protection on/off.
//! Anything needing admin reruns the `guarden` CLI elevated (one UAC prompt per action).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    let app = tauri::Builder::default().plugin(tauri_plugin_updater::Builder::new().build());
    #[cfg(windows)]
    let app = app.setup(|_| {
        // After an update the launchers still run the previous shim copy: swap it for the new one.
        if let Ok(exe) = std::env::current_exe() {
            let _ = guarden::refresh_shim(&exe.with_file_name("guarden-shim.exe"));
        }
        Ok(())
    });
    #[cfg(windows)]
    let app = app.invoke_handler(tauri::generate_handler![
        cmd::get_state,
        cmd::check_update,
        cmd::install_update,
        cmd::set_protection,
        cmd::finish_setup,
        cmd::set_diagnostic,
        cmd::open_folder,
        cmd::uninstall
    ]);
    // ponytail: Windows-only for now; on Linux the window opens but every command is missing.
    app.run(tauri::generate_context!()).expect("error while running Guarden");
}

/// The frontend's commands. All async: sync commands run on the window thread and would freeze the UI.
#[cfg(windows)]
mod cmd {
    use guarden::{modrinth, sandbox};
    use serde::Serialize;
    use std::collections::HashMap;
    use std::path::PathBuf;

    #[derive(Serialize)]
    pub struct Instance {
        name: String,
        version: String,
        /// "ok", "incomplete" (no localhost exemption yet) or "failed" (last launch exited non-zero)
        state: &'static str,
        code: Option<u32>,
        container: String,
    }

    #[derive(Serialize)]
    pub struct Launcher {
        id: &'static str,
        name: &'static str,
        mono: &'static str,
        supported: bool,
        on: bool,
        running: bool,
        instances: Vec<Instance>,
        /// Instance count for launchers Guarden can't protect yet.
        count: Option<usize>,
    }

    #[derive(Serialize)]
    pub struct Play {
        ts: u64,
        name: String,
        minutes: Option<u64>,
        code: Option<u32>,
    }

    #[derive(Serialize)]
    pub struct State {
        launchers: Vec<Launcher>,
        recent: Vec<Play>,
        diagnostic: bool,
        version: String,
    }

    /// "Minecraft 1.20.1 · Forge"
    fn version(i: &modrinth::Instance) -> String {
        let loader = match i.loader.as_deref() {
            None | Some("vanilla") => String::new(),
            Some("neoforge") => " · NeoForge".into(),
            Some(l) => format!(" · {}{}", l[..1].to_uppercase(), &l[1..]),
        };
        format!("Minecraft {}{loader}", i.game_version.as_deref().unwrap_or("?"))
    }

    #[tauri::command]
    pub async fn get_state(app: tauri::AppHandle) -> Result<State, String> {
        let launches = guarden::launches();
        let mut names = HashMap::new();
        let mut launchers = vec![];
        if modrinth::installed() {
            let st = modrinth::status().map_err(|e| e.to_string())?;
            let instances = st
                .instances
                .iter()
                .map(|i| {
                    let container = guarden::container_name(&i.game_dir);
                    names.insert(container.clone(), i.name.clone());
                    let code = launches.iter().rev().find(|l| l.container == container).and_then(|l| l.code);
                    let state = match code {
                        _ if !sandbox::has_localhost(&container) => "incomplete",
                        Some(c) if c != 0 => "failed",
                        _ => "ok",
                    };
                    Instance { name: i.name.clone(), version: version(i), state, code, container }
                })
                .collect();
            launchers.push(Launcher {
                id: "modrinth",
                name: "Modrinth App",
                mono: "Mo",
                supported: true,
                on: st.protected,
                running: modrinth::is_running(),
                instances,
                count: None,
            });
        }
        // Detected, not supported yet: shown so people know they're coming.
        let appdata = PathBuf::from(std::env::var_os("APPDATA").unwrap_or_default());
        for (id, name, mono, dir) in [
            ("prism", "Prism Launcher", "Pr", appdata.join("PrismLauncher").join("instances")),
            ("official", "Launcher officiel", "Mc", appdata.join(".minecraft")),
        ] {
            if let Ok(entries) = std::fs::read_dir(&dir) {
                let count = (id == "prism").then(|| entries.flatten().filter(|e| e.path().is_dir()).count());
                launchers.push(Launcher { id, name, mono, supported: false, on: false, running: false, instances: vec![], count });
            }
        }
        let recent = launches
            .iter()
            .rev()
            .filter_map(|l| {
                let name = names.get(&l.container)?.clone();
                Some(Play { ts: l.start, name, minutes: l.end.map(|e| e.saturating_sub(l.start) / 60), code: l.code })
            })
            .take(8)
            .collect();
        Ok(State { launchers, recent, diagnostic: guarden::root().join("debug").exists(), version: app.package_info().version.to_string() })
    }

    /// The newer version available, if any. Offline or no release yet: just None.
    #[tauri::command]
    pub async fn check_update(app: tauri::AppHandle) -> Option<String> {
        use tauri_plugin_updater::UpdaterExt;
        app.updater().ok()?.check().await.ok()?.map(|u| u.version)
    }

    /// Download, check the signature and run the installer, which closes and relaunches the app.
    #[tauri::command]
    pub async fn install_update(app: tauri::AppHandle) -> Result<(), String> {
        use tauri_plugin_updater::UpdaterExt;
        let update = app.updater().map_err(|e| e.to_string())?.check().await.map_err(|e| e.to_string())?;
        let update = update.ok_or("Guarden est déjà à jour.")?;
        update.download_and_install(|_, _| {}, || {}).await.map_err(|e| e.to_string())?;
        app.restart()
    }

    /// Rerun the `guarden` CLI (installed next to the app) as admin. Errors are shown as-is, except
    /// "cancelled" when the UAC prompt was declined.
    fn elevated(args: &str) -> Result<(), String> {
        let cli = std::env::current_exe().map_err(|e| e.to_string())?.with_file_name("guarden.exe");
        match sandbox::run_elevated(&cli, args) {
            Ok(0) => Ok(()),
            Ok(_) => Err("Guarden n’a pas pu terminer. Réessaie, et active le mode diagnostic si ça recommence.".into()),
            Err(_) => Err("cancelled".into()),
        }
    }

    fn supported(id: &str) -> Result<(), String> {
        if id == "modrinth" { Ok(()) } else { Err("Ce launcher n’est pas encore pris en charge.".into()) }
    }

    /// "running" when the launcher is open: it would overwrite its own settings over ours.
    #[tauri::command]
    pub async fn set_protection(id: String, on: bool) -> Result<(), String> {
        supported(&id)?;
        if modrinth::is_running() {
            return Err("running".into());
        }
        if on { elevated("protect modrinth") } else { modrinth::restore().map_err(|e| e.to_string()) }
    }

    /// Give new instances their container and localhost access. Works with the launcher open.
    #[tauri::command]
    pub async fn finish_setup(id: String) -> Result<(), String> {
        supported(&id)?;
        elevated("prepare modrinth")
    }

    #[tauri::command]
    pub async fn set_diagnostic(on: bool) -> Result<(), String> {
        let flag = guarden::root().join("debug");
        let r = if on { std::fs::create_dir_all(guarden::root()).and_then(|_| std::fs::write(&flag, "")) } else { std::fs::remove_file(&flag) };
        r.map_err(|e| e.to_string())
    }

    #[tauri::command]
    pub async fn open_folder() -> Result<(), String> {
        std::process::Command::new("explorer").arg(guarden::root()).spawn().map(|_| ()).map_err(|e| e.to_string())
    }

    /// Restore every launcher. ponytail: ACEs and containers stay until Restore records its grants.
    #[tauri::command]
    pub async fn uninstall() -> Result<(), String> {
        if modrinth::installed() {
            if modrinth::is_running() {
                return Err("running".into());
            }
            modrinth::restore().map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}
