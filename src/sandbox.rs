use crate::{acl, policy, policy::Policy, push_arg};
use std::error::Error;
use std::ffi::OsString;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use windows::Win32::Foundation::*;
use windows::Win32::Security::Authorization::{ConvertSidToStringSidW, ConvertStringSidToSidW};
use windows::Win32::Security::Isolation::*;
use windows::Win32::Security::*;
use windows::Win32::Storage::FileSystem::{
    FILE_ALL_ACCESS, FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, SYNCHRONIZE,
};
use windows::Win32::System::Console::*;
use windows::Win32::System::JobObjects::*;
use windows::Win32::System::SystemServices::SE_GROUP_ENABLED;
use windows::Win32::System::Threading::*;
use windows::core::{HSTRING, PCWSTR, PWSTR};

type R<T> = Result<T, Box<dyn Error>>;

/// Well-known capability SIDs: internetClient, privateNetworkClientServer (LAN / Open to LAN).
const CAPABILITIES: [&str; 2] = ["S-1-15-3-1", "S-1-15-3-3"];

// ponytail: PSIDs and handles below are never freed; the shim is one short-lived process per launch.

/// SID of the AppContainer profile `name`, creating the profile on first use.
pub fn container_sid(name: &str) -> windows::core::Result<PSID> {
    let n = HSTRING::from(name);
    unsafe {
        match CreateAppContainerProfile(&n, &n, &HSTRING::from("Guarden sandbox"), None) {
            Err(e) if e.code() == ERROR_ALREADY_EXISTS.to_hresult() => DeriveAppContainerSidFromAppContainerName(&n),
            r => r,
        }
    }
}

/// Create `container` ahead of its first launch and let it reach localhost, where the launcher
/// listens for the game (Modrinth's live log). The exemption needs admin, so it can't be in the shim.
// ponytail: exempts all of localhost; proxy just the launcher's port if a local service needs hiding.
pub fn prepare(container: &str) -> R<()> {
    let sid = unsafe {
        let mut s = PWSTR::null();
        ConvertSidToStringSidW(container_sid(container)?, &mut s)?;
        s.to_string()?
    };
    let out = std::process::Command::new("CheckNetIsolation.exe").args(["LoopbackExempt", "-a", &format!("-p={sid}")]).output()?;
    if !out.status.success() {
        return Err(format!("localhost exemption failed (run as admin): {}", String::from_utf8_lossy(&out.stdout).trim()).into());
    }
    Ok(())
}

/// Has `container` its localhost exemption (see `prepare`)? Readable without admin.
// ponytail: the SID array from the API is leaked; called a few times per refresh.
pub fn has_localhost(container: &str) -> bool {
    use windows::Win32::NetworkManagement::WindowsFirewall::NetworkIsolationGetAppContainerConfig;
    unsafe {
        let Ok(sid) = DeriveAppContainerSidFromAppContainerName(&HSTRING::from(container)) else { return false };
        let (mut n, mut sids) = (0u32, std::ptr::null_mut());
        if NetworkIsolationGetAppContainerConfig(&mut n, &mut sids) != 0 || sids.is_null() {
            return false;
        }
        std::slice::from_raw_parts(sids, n as usize).iter().any(|s| EqualSid(s.Sid, sid).is_ok())
    }
}

/// Run `exe args` as administrator (the UAC prompt), hidden, and return its exit code.
/// Err if the user declines the prompt.
pub fn run_elevated(exe: &Path, args: &str) -> R<u32> {
    use windows::Win32::UI::Shell::{SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
    use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
    let (file, params, verb) = (HSTRING::from(exe), HSTRING::from(args), HSTRING::from("runas"));
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
        lpVerb: PCWSTR(verb.as_ptr()),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(params.as_ptr()),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    unsafe {
        ShellExecuteExW(&mut info)?; // ERROR_CANCELLED when the prompt is declined
        WaitForSingleObject(info.hProcess, INFINITE);
        let mut code = 0;
        GetExitCodeProcess(info.hProcess, &mut code)?;
        let _ = CloseHandle(info.hProcess);
        Ok(code)
    }
}

/// Our own capability, carried by every Guarden container, so a dir only needs listing granted once
/// for all of them (`C:\` and `C:\Users` need admin for that: `guarden install` in an admin shell).
fn list_capability() -> windows::core::Result<PSID> {
    let (mut groups, mut ng, mut caps, mut nc) = (std::ptr::null_mut(), 0, std::ptr::null_mut(), 0);
    unsafe {
        DeriveCapabilitySidsFromName(&HSTRING::from("guardenList"), &mut groups, &mut ng, &mut caps, &mut nc)?;
        Ok(*caps)
    }
}

/// Let the containers list `dirs`: names only, no file contents, nothing below is inherited.
// ponytail: never revoked yet; Restore will, once it records what was granted.
pub fn allow_listing(dirs: &[PathBuf]) -> R<()> {
    let cap = list_capability()?;
    let mask = (FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | SYNCHRONIZE).0;
    for d in dirs {
        acl::grant(d, cap, mask, false)
            .map_err(|e| format!("can't let the game list {}: {e} (run `guarden install` once from an admin PowerShell)", d.display()))?;
    }
    Ok(())
}

/// `%LOCALAPPDATA%\Packages\<name>\AC\Temp`: the container owns it, the JVM and LWJGL extract natives there.
fn container_temp(sid: PSID) -> R<PathBuf> {
    unsafe {
        let mut s = PWSTR::null();
        ConvertSidToStringSidW(sid, &mut s)?;
        let folder = GetAppContainerFolderPath(PCWSTR(s.0))?;
        let p = PathBuf::from(folder.to_string()?).join("Temp");
        std::fs::create_dir_all(&p)?;
        Ok(p)
    }
}

/// Run `exe args` inside the AppContainer `container`, with `pol` applied, and return its exit code.
/// The child sits in a kill-on-close job: if the launcher kills us ("Stop"), java dies with us.
/// `output`: send java's stdout/stderr there instead of to the launcher (debug).
pub fn run(container: &str, exe: &Path, args: &[String], pol: &Policy, output: Option<&std::fs::File>) -> R<u32> {
    let sid = container_sid(container)?;
    for d in &pol.rw {
        acl::grant(d, sid, FILE_ALL_ACCESS.0, true)?;
    }
    for d in &pol.ro {
        acl::grant(d, sid, (FILE_GENERIC_READ | FILE_GENERIC_EXECUTE).0, true)?;
    }
    allow_listing(&pol.ancestors())?;
    let temp = container_temp(sid)?;
    let mut args = args.to_vec();
    policy::redirect_extract_dirs(&mut args, &temp);

    unsafe {
        let mut caps = CAPABILITIES
            .iter()
            .map(|s| {
                let mut p = PSID::default();
                ConvertStringSidToSidW(&HSTRING::from(*s), &mut p).map(|_| p)
            })
            .chain([list_capability()])
            .map(|p| p.map(|p| SID_AND_ATTRIBUTES { Sid: p, Attributes: SE_GROUP_ENABLED as u32 }))
            .collect::<windows::core::Result<Vec<_>>>()?;
        let sc = SECURITY_CAPABILITIES {
            AppContainerSid: sid,
            Capabilities: caps.as_mut_ptr(),
            CapabilityCount: caps.len() as u32,
            Reserved: 0,
        };

        // Only our std handles (the launcher's log pipes) may cross into the container.
        let mut std_h = [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE].map(|h| GetStdHandle(h).unwrap_or_default());
        if let Some(f) = output {
            let h = HANDLE(std::os::windows::io::AsRawHandle::as_raw_handle(f));
            [std_h[1], std_h[2]] = [h, h];
        }
        let mut inherit: Vec<HANDLE> = std_h
            .into_iter()
            .filter(|h| !h.is_invalid() && SetHandleInformation(*h, HANDLE_FLAG_INHERIT.0, HANDLE_FLAG_INHERIT).is_ok())
            .collect();
        inherit.sort_by_key(|h| h.0 as usize);
        inherit.dedup();

        let count = if inherit.is_empty() { 1 } else { 2 };
        let mut size = 0;
        let _ = InitializeProcThreadAttributeList(None, count, None, &mut size); // fails by design, returns size
        let mut buf = vec![0u8; size];
        let list = LPPROC_THREAD_ATTRIBUTE_LIST(buf.as_mut_ptr() as _);
        InitializeProcThreadAttributeList(Some(list), count, None, &mut size)?;
        UpdateProcThreadAttribute(
            list,
            0,
            PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES as usize,
            Some(&sc as *const _ as _),
            size_of::<SECURITY_CAPABILITIES>(),
            None,
            None,
        )?;
        if !inherit.is_empty() {
            UpdateProcThreadAttribute(
                list,
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                Some(inherit.as_ptr() as _),
                inherit.len() * size_of::<HANDLE>(),
                None,
                None,
            )?;
        }

        let mut si = STARTUPINFOEXW::default();
        si.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        si.lpAttributeList = list;
        if !inherit.is_empty() {
            si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
            [si.StartupInfo.hStdInput, si.StartupInfo.hStdOutput, si.StartupInfo.hStdError] = std_h;
        }

        let mut cmd = String::new();
        push_arg(&mut cmd, &exe.to_string_lossy());
        for a in &args {
            push_arg(&mut cmd, a);
        }
        let mut cmd: Vec<u16> = cmd.encode_utf16().chain([0]).collect();
        let env = env_block(&temp);
        // ponytail: cwd = gameDir (what launchers use); the inherited cwd may not be readable from the container.
        let cwd = pol.rw.first().map(|d| HSTRING::from(d.as_path())).unwrap_or_default();

        let mut pi = PROCESS_INFORMATION::default();
        let created = CreateProcessW(
            &HSTRING::from(exe),
            Some(PWSTR(cmd.as_mut_ptr())),
            None,
            None,
            !inherit.is_empty(),
            EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | CREATE_SUSPENDED,
            Some(env.as_ptr() as _),
            if cwd.is_empty() { PCWSTR::null() } else { PCWSTR(cwd.as_ptr()) },
            &si.StartupInfo,
            &mut pi,
        );
        DeleteProcThreadAttributeList(list);
        created?;

        let job = CreateJobObjectW(None, PCWSTR::null())?;
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let jobbed = SetInformationJobObject(job, JobObjectExtendedLimitInformation, &info as *const _ as _, size_of_val(&info) as u32)
            .and_then(|_| AssignProcessToJobObject(job, pi.hProcess));
        if let Err(e) = jobbed {
            let _ = TerminateProcess(pi.hProcess, 1);
            return Err(e.into());
        }
        ResumeThread(pi.hThread);
        WaitForSingleObject(pi.hProcess, INFINITE);
        let mut code = 0;
        GetExitCodeProcess(pi.hProcess, &mut code)?;
        Ok(code)
    }
}

/// Our environment with TEMP/TMP pointed at the container's own temp dir.
fn env_block(temp: &Path) -> Vec<u16> {
    let mut vars: Vec<(OsString, OsString)> = std::env::vars_os()
        .filter(|(k, _)| !k.eq_ignore_ascii_case("TEMP") && !k.eq_ignore_ascii_case("TMP"))
        .collect();
    vars.push(("TEMP".into(), temp.into()));
    vars.push(("TMP".into(), temp.into()));
    vars.sort_by_key(|(k, _)| k.to_ascii_uppercase());
    vars.iter()
        .flat_map(|(k, v)| k.encode_wide().chain([b'=' as u16]).chain(v.encode_wide()).chain([0]))
        .chain([0])
        .collect()
}
