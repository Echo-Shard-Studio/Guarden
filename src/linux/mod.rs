//! Linux: the game runs under Landlock (it only sees what the policy grants, plus the system dirs it
//! needs) and a seccomp filter that hands each of its connect() calls to us. Below Landlock ABI 9 the
//! kernel doesn't restrict connecting to named UNIX sockets: without that check the game could reach
//! the session D-Bus (`systemd-run --user`: out of the sandbox). We connect for it, our own copy of the
//! address, so it can't swap it after the check: anything but UNIX goes, UNIX only to the display and
//! sound sockets. Nothing is written to disk: no setup, nothing to clean up, no admin.
// ponytail: localhost is reachable (Landlock filters TCP by port, not address); a network namespace
// needs bubblewrap, which doesn't run inside the launchers' Flatpak.
use crate::R;
use crate::policy::{self, Policy};
use landlock::{ABI, Access, AccessFs, RestrictionStatus, Ruleset, RulesetAttr, RulesetCreatedAttr, RulesetError, RulesetStatus, Scope, path_beneath_rules};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

/// Newest Landlock ABI we know; older kernels enforce what they support (best effort).
const LANDLOCK: ABI = ABI::V9;

/// Exe names of a java (fake javas get these).
pub const JAVAS: [&str; 1] = ["java"];
/// The one launchers are pointed at.
pub const JAVA: &str = "java";

/// Read-only for the game: libraries, config, GPU and device info. `/app` is the Flatpak's own files.
const SYSTEM: [&str; 17] = [
    "/usr", "/bin", "/sbin", "/lib", "/lib32", "/lib64", "/etc", "/app", "/run/host/monitor", "/sys",
    // /proc/self is resolved in the game's process: its own entry only, not the others' command lines.
    "/proc/self", "/proc/cpuinfo", "/proc/meminfo", "/proc/stat", "/proc/loadavg", "/proc/filesystems", "/proc/sys",
];
/// Devices it may open (GPU). Not /dev as a whole: webcams, input devices.
const DEVICES: [&str; 6] = ["/dev/null", "/dev/zero", "/dev/full", "/dev/random", "/dev/urandom", "/dev/dri"];

pub fn home() -> PathBuf {
    crate::env_path("HOME").unwrap_or_default()
}

/// Where apps keep their data (in a Flatpak: its own, under `~/.var/app/<id>`).
pub fn data_dir() -> PathBuf {
    crate::env_path("XDG_DATA_HOME").unwrap_or_else(|| home().join(".local/share"))
}

/// Where apps keep their settings (Electron ones too).
pub fn config_dir() -> PathBuf {
    crate::env_path("XDG_CONFIG_HOME").unwrap_or_else(|| home().join(".config"))
}

/// Never exposed to the game, nor any dir above them (see `deny_list`).
pub fn private_dirs() -> Vec<PathBuf> {
    vec![home(), data_dir(), config_dir(), home().join(".var/app")]
}

/// Run `exe args` sandboxed and return its exit code. `container` names its temp dir. The launcher
/// keeps java's stdout/stderr and its kill (we die, it dies). `output`, `input`: as on Windows.
pub fn run(container: &str, exe: &Path, args: &[String], pol: &Policy, output: Option<&std::fs::File>, input: Option<Vec<u8>>) -> R<u32> {
    let temp = crate::root().join("tmp").join(container);
    std::fs::create_dir_all(&temp)?;
    let mut args = args.to_vec();
    policy::redirect_extract_dirs(&mut args, &temp);
    // java ignores TMPDIR; LWJGL extracts there. JNA would use ~/.cache/JNA.
    args.splice(0..0, [format!("-Djava.io.tmpdir={}", temp.display()), format!("-Djna.tmpdir={}", temp.display())]);

    let mut cmd = Command::new(exe);
    cmd.args(&args).env("TMPDIR", &temp).env("XDG_CACHE_HOME", &temp).env("MESA_SHADER_CACHE_DIR", &temp).env("__GL_SHADER_DISK_CACHE_PATH", &temp);
    if let Some(f) = output {
        cmd.stdout(f.try_clone()?).stderr(f.try_clone()?);
    }
    if input.is_some() {
        cmd.stdin(Stdio::piped());
    }
    let rw: Vec<PathBuf> = pol.rw.iter().cloned().chain([temp]).collect();
    let mut ro: Vec<PathBuf> = pol.ro.iter().cloned().chain(SYSTEM.map(PathBuf::from)).collect();
    ro.push(crate::env_path("XAUTHORITY").unwrap_or_else(|| home().join(".Xauthority")));
    let (ours, theirs) = UnixStream::pair()?;
    let (send, parent) = (theirs.as_raw_fd(), std::process::id());
    // Runs in the child between fork and exec; we're single-threaded here, so allocating is fine.
    unsafe { cmd.pre_exec(move || confine(&rw, &ro, parent, send)) };
    let mut child = cmd.spawn()?;
    drop(theirs);

    let listener = Arc::new(recv_fd(&ours)?);
    let allowed = Arc::new(allowed_sockets());
    std::thread::spawn(move || supervise(listener, allowed));
    if let (Some(head), Some(mut stdin)) = (input, child.stdin.take()) {
        std::thread::spawn(move || {
            let _ = stdin.write_all(&head).and_then(|_| io::copy(&mut io::stdin(), &mut stdin));
        });
    }
    let st = child.wait()?;
    Ok(st.code().map_or(128 + st.signal().unwrap_or(0) as u32, |c| c as u32))
}

/// In the child, before exec: tie it to us, then Landlock, then the connect() filter.
fn confine(rw: &[PathBuf], ro: &[PathBuf], parent: u32, send: RawFd) -> io::Result<()> {
    unsafe {
        libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL); // "Stop" in the launcher kills us
        if libc::getppid() as u32 != parent {
            return Err(io::Error::other("the shim exited"));
        }
        libc::setsid(); // no controlling terminal: TIOCSTI can't type into the launcher's shell
    }
    if landlock(rw, ro).map_err(io::Error::other)?.ruleset == RulesetStatus::NotEnforced {
        return Err(io::Error::other("this kernel has no Landlock (5.13+, enabled in the LSM list): the game can't be sandboxed"));
    }
    let listener = connect_filter()?;
    send_fd(send, listener.as_raw_fd())?;
    // Whatever fds the launcher leaked to us: closed at exec.
    unsafe { libc::syscall(libc::SYS_close_range, 3, u32::MAX, libc::CLOSE_RANGE_CLOEXEC) };
    Ok(())
}

fn landlock(rw: &[PathBuf], ro: &[PathBuf]) -> Result<RestrictionStatus, RulesetError> {
    let read = AccessFs::from_read(LANDLOCK);
    let nvidia = std::fs::read_dir("/dev").into_iter().flatten().flatten().map(|d| d.path());
    let devices: Vec<PathBuf> = DEVICES.map(PathBuf::from).into_iter().chain(nvidia.filter(|p| p.to_string_lossy().starts_with("/dev/nvidia"))).collect();
    Ruleset::default()
        .handle_access(AccessFs::from_all(LANDLOCK))?
        .scope(Scope::from_all(LANDLOCK))? // no signals to, nor abstract sockets of, processes outside
        .create()?
        .add_rules(path_beneath_rules(rw, AccessFs::from_all(LANDLOCK)))?
        .add_rules(path_beneath_rules(ro, read))?
        .add_rules(path_beneath_rules(devices, read | AccessFs::WriteFile | AccessFs::IoctlDev))?
        .restrict_self()
}

/// connect() → us (`supervise`); io_uring (could connect around it) and UNIX sockets other than
/// stream ones (their sendto() takes an address) refused. Returns the listener fd.
fn connect_filter() -> io::Result<OwnedFd> {
    use libc::*;
    #[cfg(target_arch = "x86_64")]
    const ARCH: u32 = 0xc000_003e;
    #[cfg(target_arch = "aarch64")]
    const ARCH: u32 = 0xc000_00b7;
    let op = |code: u32, k: u32, jt: u8, jf: u8| sock_filter { code: code as u16, jt, jf, k };
    let (ld, jeq, ret) = (BPF_LD | BPF_W | BPF_ABS, BPF_JMP | BPF_JEQ | BPF_K, BPF_RET | BPF_K);
    let prog = [
        op(ld, 4, 0, 0), // arch
        op(jeq, ARCH, 1, 0),
        op(ret, SECCOMP_RET_KILL_PROCESS, 0, 0),
        op(ld, 0, 0, 0), // syscall nr
        op(BPF_JMP | BPF_JGE | BPF_K, 0x4000_0000, 0, 1), // x32 numbering on x86_64
        op(ret, SECCOMP_RET_KILL_PROCESS, 0, 0),
        op(jeq, SYS_connect as u32, 0, 1),
        op(ret, SECCOMP_RET_USER_NOTIF, 0, 0),
        op(jeq, SYS_io_uring_setup as u32, 0, 1),
        op(ret, SECCOMP_RET_ERRNO | ENOSYS as u32, 0, 0),
        op(jeq, SYS_socket as u32, 1, 0),
        op(ret, SECCOMP_RET_ALLOW, 0, 0),
        op(ld, 16, 0, 0), // args[0] (low half): domain
        op(jeq, AF_UNIX as u32, 1, 0),
        op(ret, SECCOMP_RET_ALLOW, 0, 0),
        op(ld, 24, 0, 0), // args[1]: type, with SOCK_NONBLOCK/CLOEXEC
        op(BPF_ALU | BPF_AND | BPF_K, 0xf, 0, 0),
        op(jeq, SOCK_STREAM as u32, 0, 1),
        op(ret, SECCOMP_RET_ALLOW, 0, 0),
        op(ret, SECCOMP_RET_ERRNO | EACCES as u32, 0, 0),
    ];
    let fprog = sock_fprog { len: prog.len() as u16, filter: prog.as_ptr() as *mut _ };
    // no_new_privs, required here, was set by Landlock's restrict_self.
    let fd = unsafe { syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER, SECCOMP_FILTER_FLAG_NEW_LISTENER, &fprog) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
}

/// Display (Wayland, X11) and sound (PulseAudio, PipeWire) sockets: the only UNIX ones the game gets.
// ponytail: X11 lets any client read the keyboard and screen of other X11 windows; fine on Wayland.
fn allowed_sockets() -> Vec<Vec<u8>> {
    let var = |v: &str| std::env::var_os(v).filter(|s| !s.is_empty());
    let run = crate::env_path("XDG_RUNTIME_DIR").unwrap_or_default();
    let mut v = vec![run.join(var("WAYLAND_DISPLAY").unwrap_or("wayland-0".into())), run.join(var("PIPEWIRE_REMOTE").unwrap_or("pipewire-0".into()))];
    v.push(run.join("pulse/native"));
    v.extend(var("PULSE_SERVER").and_then(|s| s.into_string().ok()?.strip_prefix("unix:").map(PathBuf::from))); // Flatpak
    let display = var("DISPLAY").and_then(|d| d.into_string().ok());
    if let Some(n) = display.as_deref().and_then(|d| d.strip_prefix(':')) {
        v.push(format!("/tmp/.X11-unix/X{}", n.split('.').next().unwrap_or(n)).into());
    }
    v.into_iter().filter(|p| p.is_absolute()).map(|p| p.into_os_string().into_vec()).collect()
}

/// Answer the game's connect() calls until it exits. One thread each: a slow TCP connect mustn't
/// hold up the others.
fn supervise(listener: Arc<OwnedFd>, allowed: Arc<Vec<Vec<u8>>>) {
    loop {
        let mut req: libc::seccomp_notif = unsafe { std::mem::zeroed() };
        if unsafe { libc::ioctl(listener.as_raw_fd(), libc::SECCOMP_IOCTL_NOTIF_RECV, &mut req) } < 0 {
            match io::Error::last_os_error().raw_os_error() {
                Some(libc::EINTR) => continue,
                _ => return, // the game is gone
            }
        }
        let (listener, allowed) = (listener.clone(), allowed.clone());
        std::thread::spawn(move || {
            let error = connect_for(listener.as_raw_fd(), &req, &allowed).err().unwrap_or(0);
            let mut resp = libc::seccomp_notif_resp { id: req.id, val: 0, error: -error, flags: 0 };
            unsafe { libc::ioctl(listener.as_raw_fd(), libc::SECCOMP_IOCTL_NOTIF_SEND, &mut resp) }; // fails if it was interrupted: fine
        });
    }
}

/// Do the game's `connect(fd, addr, len)` on its socket, with our copy of `addr`. Err = errno.
fn connect_for(listener: RawFd, req: &libc::seccomp_notif, allowed: &[Vec<u8>]) -> Result<(), i32> {
    let errno = || io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO);
    let [fd, addr, len, ..] = req.data.args;
    let mut buf = [0u8; size_of::<libc::sockaddr_storage>()];
    let len = (len as usize).min(buf.len());
    let local = libc::iovec { iov_base: buf.as_mut_ptr().cast(), iov_len: len };
    let remote = libc::iovec { iov_base: addr as *mut _, iov_len: len };
    if len < 2 || unsafe { libc::process_vm_readv(req.pid as i32, &local, 1, &remote, 1, 0) } != len as isize {
        return Err(libc::EFAULT);
    }
    if u16::from_ne_bytes([buf[0], buf[1]]) == libc::AF_UNIX as u16 {
        let path = &buf[2..len];
        let path = &path[..path.iter().position(|&b| b == 0).unwrap_or(path.len())]; // abstract: empty, refused
        if !allowed.iter().any(|a| a == path) {
            // As if nothing listened: clients then try their next address (xcb: the abstract X11
            // socket, then the file one) where EACCES would make them give up.
            return Err(libc::ECONNREFUSED);
        }
    }
    // req.pid is a thread; pidfd_open wants its process.
    let status = std::fs::read_to_string(format!("/proc/{}/status", req.pid)).map_err(|_| libc::ESRCH)?;
    let tgid: i32 = status.lines().find_map(|l| l.strip_prefix("Tgid:")).and_then(|t| t.trim().parse().ok()).ok_or(libc::ESRCH)?;
    let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, tgid, 0) };
    if pidfd < 0 {
        return Err(errno());
    }
    let pidfd = unsafe { OwnedFd::from_raw_fd(pidfd as RawFd) };
    // Still waiting on us, so that pid wasn't reused by another process since we read from it.
    if unsafe { libc::ioctl(listener, libc::SECCOMP_IOCTL_NOTIF_ID_VALID, &req.id) } < 0 {
        return Err(libc::ESRCH);
    }
    let sock = unsafe { libc::syscall(libc::SYS_pidfd_getfd, pidfd.as_raw_fd(), fd as RawFd, 0) };
    if sock < 0 {
        return Err(errno());
    }
    let sock = unsafe { OwnedFd::from_raw_fd(sock as RawFd) };
    if unsafe { libc::connect(sock.as_raw_fd(), buf.as_ptr().cast(), len as u32) } < 0 {
        return Err(errno()); // EINPROGRESS for a non-blocking socket, as the game expects
    }
    Ok(())
}

/// Space for one fd in a control message, aligned for `cmsghdr`.
#[repr(C)]
union Cmsg {
    buf: [u8; 32],
    _align: libc::cmsghdr,
}

fn fd_msg(byte: &mut u8, cmsg: &mut Cmsg) -> (libc::msghdr, libc::iovec) {
    let iov = libc::iovec { iov_base: (byte as *mut u8).cast(), iov_len: 1 };
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_control = (cmsg as *mut Cmsg).cast();
    msg.msg_controllen = unsafe { libc::CMSG_SPACE(size_of::<RawFd>() as u32) } as _;
    (msg, iov)
}

fn send_fd(sock: RawFd, fd: RawFd) -> io::Result<()> {
    let (mut byte, mut cmsg) = (0, Cmsg { buf: [0; 32] });
    let (mut msg, mut iov) = fd_msg(&mut byte, &mut cmsg);
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    unsafe {
        let c = libc::CMSG_FIRSTHDR(&msg);
        (*c).cmsg_level = libc::SOL_SOCKET;
        (*c).cmsg_type = libc::SCM_RIGHTS;
        (*c).cmsg_len = libc::CMSG_LEN(size_of::<RawFd>() as u32) as _;
        std::ptr::write_unaligned(libc::CMSG_DATA(c).cast(), fd);
        if libc::sendmsg(sock, &msg, 0) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn recv_fd(sock: &UnixStream) -> io::Result<OwnedFd> {
    let (mut byte, mut cmsg) = (0, Cmsg { buf: [0; 32] });
    let (mut msg, mut iov) = fd_msg(&mut byte, &mut cmsg);
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    unsafe {
        if libc::recvmsg(sock.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC) < 1 {
            return Err(io::Error::other("the game started without its connect() filter"));
        }
        let c = libc::CMSG_FIRSTHDR(&msg);
        if c.is_null() || (*c).cmsg_type != libc::SCM_RIGHTS {
            return Err(io::Error::other("no filter fd from the game"));
        }
        Ok(OwnedFd::from_raw_fd(std::ptr::read_unaligned(libc::CMSG_DATA(c).cast())))
    }
}

/// No admin on Linux: just runs it.
pub fn run_elevated(exe: &Path, args: &str) -> R<u32> {
    let st = Command::new(exe).args(args.split(' ')).status()?;
    Ok(st.code().unwrap_or(1) as u32)
}

/// Nothing to prepare: no profiles, and localhost isn't blocked (see the top).
pub fn prepare(_: &str) -> R<()> {
    Ok(())
}

pub fn has_localhost(_: &str) -> bool {
    true
}

/// Landlock doesn't restrict walking paths: nothing to allow.
pub fn allow_listing(_: &[PathBuf]) -> R<()> {
    Ok(())
}

/// Is any of these processes running? (exe names, case-insensitive, `.exe` dropped)
pub fn running(names: &[&str]) -> bool {
    let want: Vec<String> = names.iter().map(|n| n.to_lowercase().trim_end_matches(".exe").to_string()).collect();
    let exes = std::fs::read_dir("/proc").into_iter().flatten().flatten().filter_map(|d| std::fs::read_link(d.path().join("exe")).ok());
    exes.into_iter().any(|e| e.file_name().is_some_and(|f| want.contains(&f.to_string_lossy().to_lowercase())))
}

pub fn open(dir: &Path) -> io::Result<()> {
    Command::new("xdg-open").arg(dir).spawn().map(drop)
}
