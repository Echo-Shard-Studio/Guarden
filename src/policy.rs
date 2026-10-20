use std::path::{Path, PathBuf};

/// Filesystem grants for one launch. Everything else stays invisible to the container.
#[derive(Debug, Default, PartialEq)]
pub struct Policy {
    pub rw: Vec<PathBuf>,
    pub ro: Vec<PathBuf>,
}

/// Derive grants from the java command line the launcher sent, so it works with any launcher:
/// `--gameDir` gets read/write, and any existing absolute path in the args (classpath entries,
/// `-D...=path`, `-javaagent:path`, `--assetsDir`...) gets read-only, along with `java_home`.
/// Refuses if a grant would expose something in `deny` (the grant itself, or an ancestor of it).
pub fn from_args(args: &[String], java_home: &Path, deny: &[PathBuf]) -> Result<Policy, String> {
    let mut p = Policy::default();
    for (i, a) in args.iter().enumerate() {
        if a == "--gameDir"
            && let Some(d) = args.get(i + 1)
        {
            p.rw.push(d.into());
        }
        let agent = a.strip_prefix("-javaagent:").map(|v| v.split('=').next().unwrap_or(v));
        let after_eq = a.split_once('=').map(|(_, v)| v);
        // ponytail: `@argfile` contents aren't parsed; add if a launcher turns out to use them.
        for piece in [Some(a.as_str()), agent, after_eq].into_iter().flatten().flat_map(|v| v.split(';')) {
            let path = Path::new(piece);
            if path.is_absolute() && path.exists() {
                p.ro.push(path.into());
            }
        }
    }
    p.ro.push(java_home.into());
    p.ro.retain(|r| !p.rw.iter().any(|w| r.starts_with(w)));
    p.ro.sort();
    p.ro.dedup();

    let deny: Vec<PathBuf> = deny.iter().map(|d| lc(d)).collect();
    for g in p.rw.iter().chain(&p.ro) {
        if let Some(d) = deny.iter().find(|d| d.starts_with(lc(g))) {
            return Err(format!("refusing to expose {} to the game (it contains {})", g.display(), d.display()));
        }
    }
    Ok(p)
}

/// Natives extraction dirs (JNA, LWJGL, Netty). Launchers share one per game version; writable,
/// a mod could plant a DLL there that later runs unsandboxed. So each container extracts into `to`.
pub fn redirect_extract_dirs(args: &mut [String], to: &Path) {
    const PROPS: [&str; 3] = ["-Djna.tmpdir=", "-Dorg.lwjgl.system.SharedLibraryExtractPath=", "-Dio.netty.native.workdir="];
    for a in args {
        if let Some(p) = PROPS.iter().find(|p| a.starts_with(**p)) {
            *a = format!("{p}{}", to.display());
        }
    }
}

impl Policy {
    /// Dirs the game must be able to list (names only): Java 25's `toRealPath` looks up each path
    /// component in its parent, so every ancestor of a grant, unless a directory grant covers it.
    pub fn ancestors(&self) -> Vec<PathBuf> {
        let grants: Vec<&PathBuf> = self.rw.iter().chain(&self.ro).collect();
        let mut v: Vec<PathBuf> = grants
            .iter()
            .flat_map(|g| g.ancestors().skip(1))
            .filter(|a| !grants.iter().any(|g| g.is_dir() && a.starts_with(g)))
            .map(Path::to_path_buf)
            .collect();
        v.sort();
        v.dedup();
        v
    }
}

/// Windows paths are case-insensitive; compare them lowercased.
fn lc(p: &Path) -> PathBuf {
    p.to_string_lossy().to_lowercase().into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn grants_from_minecraft_args() {
        let t = std::env::temp_dir().join(format!("guarden-test-{}", std::process::id()));
        let (game, libs, natives, java) = (t.join("game"), t.join("libs"), t.join("natives"), t.join("java"));
        for d in [&game, &libs, &natives, &java] {
            fs::create_dir_all(d).unwrap();
        }
        let (a, b) = (libs.join("a.jar"), libs.join("b.jar"));
        fs::write(&a, "").unwrap();
        fs::write(&b, "").unwrap();
        let s = |p: &Path| p.to_string_lossy().into_owned();
        let args = vec![
            format!("-Djava.library.path={}", s(&natives)),
            "-cp".into(),
            format!("{};{}", s(&a), s(&b)),
            "net.minecraft.client.main.Main".into(),
            "--gameDir".into(),
            s(&game),
            "--assetsDir".into(),
            s(&t.join("missing")),
        ];
        let deny = vec![t.join("secret")];

        let p = from_args(&args, &java, &deny).unwrap();
        assert_eq!(p.rw, vec![game.clone()]);
        assert_eq!(p.ro, vec![java.clone(), a.clone(), b.clone(), natives.clone()]);
        let anc = p.ancestors();
        assert!(anc.contains(&libs) && anc.contains(&t) && anc.contains(&PathBuf::from("/")));
        assert!(!anc.contains(&game) && !anc.contains(&a));

        let mut r = vec!["-Djna.tmpdir=X:/shared/jna".to_string(), "-Xmx1G".into()];
        redirect_extract_dirs(&mut r, Path::new("T"));
        assert_eq!(r, ["-Djna.tmpdir=T", "-Xmx1G"]);

        // granting the parent of a denied dir must be refused
        let mut bad = args.clone();
        bad.push(s(&t));
        assert!(from_args(&bad, &java, &deny).is_err());

        fs::remove_dir_all(&t).unwrap();
    }
}
