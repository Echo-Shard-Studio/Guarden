//! Guarden desktop app. Shows the launchers and their instances, and switches protection on/off.
//! Anything needing admin reruns the `guarden` CLI elevated (one UAC prompt per action).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    let app = tauri::Builder::default().plugin(tauri_plugin_updater::Builder::new().build());
    let app = app.setup(|_| {
        // After an update the launchers still run the previous shim copy: swap it for the new one.
        if let Ok(exe) = std::env::current_exe() {
            let _ = guarden::update_shims(&exe.with_file_name(format!("guarden-shim{}", std::env::consts::EXE_SUFFIX)), false);
        }
        Ok(())
    });
    let app = app.invoke_handler(tauri::generate_handler![
        cmd::get_state,
        cmd::check_update,
        cmd::install_update,
        cmd::set_protection,
        cmd::finish_setup,
        cmd::separate,
        cmd::set_diagnostic,
        cmd::open_folder,
        cmd::uninstall
    ]);
    app.run(tauri::generate_context!()).expect("error while running Guarden");
}

/// The frontend's commands. All async: sync commands run on the window thread and would freeze the UI.
mod cmd {
    use guarden::{Issue, Launcher as _, sys};
    use serde::Serialize;
    use std::collections::HashMap;
    use std::path::PathBuf;

    #[derive(Serialize)]
    pub struct Instance {
        key: String,
        name: String,
        version: String,
        /// "ok", "incomplete" (no localhost exemption yet), "failed" (last launch exited non-zero),
        /// "bypass" (launches without Guarden) or "shared" (needs its own folder first)
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
        /// Switching it on asks for admin (the UAC prompt).
        admin: bool,
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
    fn version(i: &guarden::Instance) -> String {
        let loader = match i.loader.as_deref() {
            None | Some("vanilla") => String::new(),
            Some("neoforge") => " · NeoForge".into(),
            Some(l) => format!(" · {}{}", l[..1].to_uppercase(), &l[1..]),
        };
        match i.game_version.as_deref() {
            Some(v) => format!("Minecraft {v}{loader}"),
            None if loader.is_empty() => "Minecraft".into(),
            None => loader[" · ".len()..].into(),
        }
    }

    fn mono(id: &str) -> &'static str {
        match id {
            "modrinth" => "Mo",
            "prism" => "Pr",
            "curseforge" => "Cf",
            _ => "Mc",
        }
    }

    #[tauri::command]
    pub async fn get_state(app: tauri::AppHandle) -> Result<State, String> {
        let launches = guarden::launches();
        let mut names = HashMap::new();
        let mut launchers = vec![];
        for l in guarden::launchers().into_iter().filter(|l| l.installed()) {
            let st = l.status().map_err(|e| format!("{} : {e}", l.name()))?;
            let instances = st
                .instances
                .iter()
                .map(|i| {
                    let container = guarden::container_name(&i.game_dir);
                    names.insert(container.clone(), i.name.clone());
                    let code = launches.iter().rev().find(|l| l.container == container).and_then(|l| l.code);
                    let state = match (i.issue, code) {
                        (Some(Issue::Shared), _) => "shared",
                        (Some(Issue::Bypass), _) => "bypass",
                        _ if l.localhost() && !sys::has_localhost(&container) => "incomplete",
                        (_, Some(c)) if c != 0 => "failed",
                        _ => "ok",
                    };
                    Instance { key: i.key.clone(), name: i.name.clone(), version: version(i), state, code, container }
                })
                .collect();
            launchers.push(Launcher {
                id: l.id(),
                name: l.name(),
                mono: mono(l.id()),
                supported: true,
                admin: l.localhost(),
                on: st.protected,
                running: l.is_running(),
                instances,
                count: None,
            });
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
        match sys::run_elevated(&exe("guarden")?, args) {
            Ok(0) => Ok(()),
            Ok(_) => Err("Guarden n’a pas pu terminer. Réessaie, et active le mode diagnostic si ça recommence.".into()),
            Err(_) => Err("cancelled".into()),
        }
    }

    /// A file shipped next to the app.
    fn exe(name: &str) -> Result<PathBuf, String> {
        Ok(std::env::current_exe().map_err(|e| e.to_string())?.with_file_name(format!("{name}{}", std::env::consts::EXE_SUFFIX)))
    }

    fn launcher(id: &str) -> Result<&'static dyn guarden::Launcher, String> {
        guarden::launcher(id).ok_or("Ce launcher n’est pas encore pris en charge.".into())
    }

    /// "running" when the launcher is open: it would overwrite its own settings over ours.
    /// Switching on again also picks up instances created since (`bypass`).
    #[tauri::command]
    pub async fn set_protection(id: String, on: bool) -> Result<(), String> {
        let l = launcher(&id)?;
        if l.is_running() {
            return Err("running".into());
        }
        if !on {
            return l.restore().map_err(|e| e.to_string());
        }
        if l.localhost() {
            return elevated(&format!("protect {id}"));
        }
        guarden::update_shims(&exe("guarden-shim")?, true).map_err(|e| e.to_string())?;
        l.protect().map(drop).map_err(|e| e.to_string())
    }

    /// Give new instances their container and localhost access. Works with the launcher open.
    #[tauri::command]
    pub async fn finish_setup(id: String) -> Result<(), String> {
        launcher(&id)?;
        elevated(&format!("prepare {id}"))
    }

    /// Give a vanilla installation sharing `.minecraft` its own folder (see `vanilla::separate`).
    #[tauri::command]
    pub async fn separate(key: String) -> Result<(), String> {
        if guarden::vanilla::Vanilla.is_running() {
            return Err("running".into());
        }
        guarden::vanilla::separate(&key).map(drop).map_err(|e| e.to_string())
    }

    #[tauri::command]
    pub async fn set_diagnostic(on: bool) -> Result<(), String> {
        let flag = guarden::root().join("debug");
        let r = if on { std::fs::create_dir_all(guarden::root()).and_then(|_| std::fs::write(&flag, "")) } else { std::fs::remove_file(&flag) };
        r.map_err(|e| e.to_string())
    }

    #[tauri::command]
    pub async fn open_folder() -> Result<(), String> {
        sys::open(&guarden::root()).map_err(|e| e.to_string())
    }

    /// Restore every launcher. ponytail: ACEs and containers stay until Restore records its grants.
    #[tauri::command]
    pub async fn uninstall() -> Result<(), String> {
        let installed = guarden::launchers().into_iter().filter(|l| l.installed());
        if installed.clone().any(|l| l.is_running()) {
            return Err("running".into());
        }
        installed.into_iter().try_for_each(|l| l.restore()).map_err(|e| e.to_string())
    }
}
