//! macOS: the kernel's own process calls, used the way tmux (osdep-darwin.c),
//! htop and psutil use them. The answers match what /proc gives on Linux, for
//! processes of the same user.

use std::ffi::{CStr, c_char, c_int, c_void};
use std::mem::{size_of, zeroed};
use std::path::PathBuf;

/// One `proc_pidinfo` answer, when the kernel gives all of it.
///
/// # Safety
/// `T` must be the plain C struct the kernel fills for `flavor`.
unsafe fn pidinfo<T>(pid: i32, flavor: c_int) -> Option<T> {
    let mut out: T = unsafe { zeroed() };
    let size = size_of::<T>() as c_int;
    let n = unsafe { libc::proc_pidinfo(pid, flavor, 0, (&raw mut out).cast(), size) };
    (n == size).then_some(out)
}

/// Kernel start time of a live pid, used to reject reused pids: set at fork
/// and kept across exec, as on Linux. Zombies count as gone: this call
/// doesn't see them (tmux keeps a dead pane's process unreaped).
pub fn start_time(pid: i32) -> Option<String> {
    let i: libc::proc_bsdinfo = unsafe { pidinfo(pid, libc::PROC_PIDTBSDINFO)? };
    Some(format!("{}.{:06}", i.pbi_start_tvsec, i.pbi_start_tvusec))
}

/// `kinfo_proc` as far as `kp_proc.p_stat`: after a 16-byte union, two
/// pointers and an int (xnu bsd/sys/proc.h, `struct extern_proc`). The rest
/// is room for the whole struct, which the kernel copies only if it fits.
#[repr(C)]
struct KinfoProc {
    _head: [u64; 4],
    _p_flag: c_int,
    p_stat: c_char,
    _rest: [u8; 1024 - 37],
}

/// A process that has exited but not been reaped. `kern.proc.pid` lists
/// zombies, which `proc_pidinfo` doesn't.
pub fn is_zombie(pid: i32) -> bool {
    let mut kp: KinfoProc = unsafe { zeroed() };
    let mut len = size_of::<KinfoProc>();
    let mut mib = [libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_PID, pid];
    let ok = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            4,
            (&raw mut kp).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    } == 0;
    ok && len > 36 && kp.p_stat as u32 == libc::SZOMB
}

/// The strings the kernel keeps from a process's exec (`kern.procargs2`):
/// argc, the executable path and its padding, argv, then the environment.
fn procargs(pid: i32) -> Option<Vec<u8>> {
    let mut argmax: c_int = 0;
    let mut len = size_of::<c_int>();
    let mut mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            2,
            (&raw mut argmax).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    } != 0
        || argmax <= 0
    {
        return None;
    }
    let mut buf = vec![0u8; argmax as usize];
    let mut len = buf.len();
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buf.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return None;
    }
    buf.truncate(len);
    Some(buf)
}

/// The kernel's own strings, which follow the environment (xnu
/// bsd/kern/kern_exec.c, `exec_add_apple_strings`). Usually a run of NULs
/// ends the environment first, but not when it happens to end aligned.
const APPLE: &[&str] = &[
    "executable_path=",
    "pfz=",
    "stack_guard=",
    "malloc_entropy=",
    "ptr_munge=",
    "main_stack=",
    "executable_file=",
    "dyld_file=",
    "executable_cdhash=",
    "dyld_flags=",
    "subsystem_root_path=",
    "executable_boothash=",
    "th_port=",
    "security_config=",
];

/// argv and the environment out of a `procargs` buffer, skipping padding
/// the way htop does (darwin/Platform.c): node's process.title overwrites
/// argv in place with the title and zeros, and the environment still
/// starts after them.
fn split_procargs(buf: &[u8]) -> (Vec<String>, Vec<String>) {
    let lossy = |s: &[u8]| String::from_utf8_lossy(s).into_owned();
    let Some(argc) = buf
        .get(..4)
        .map(|b| i32::from_ne_bytes([b[0], b[1], b[2], b[3]]))
    else {
        return Default::default();
    };
    let rest = &buf[4..];
    let Some(exe_end) = rest.iter().position(|&b| b == 0) else {
        return Default::default();
    };
    let mut at = exe_end;
    let skip_nuls = |mut at: usize| {
        while rest.get(at) == Some(&0) {
            at += 1;
        }
        at
    };
    at = skip_nuls(at);
    let mut argv = Vec::new();
    for _ in 0..argc.max(0) {
        if at >= rest.len() {
            break;
        }
        let end = rest[at..]
            .iter()
            .position(|&b| b == 0)
            .map_or(rest.len(), |p| at + p);
        argv.push(lossy(&rest[at..end]));
        at = end + 1;
    }
    at = skip_nuls(at);
    let env = rest
        .get(at..)
        .unwrap_or_default()
        .split(|&b| b == 0)
        .take_while(|s| !s.is_empty())
        .map(lossy)
        .take_while(|s| !APPLE.iter().any(|k| s.starts_with(k)))
        .collect();
    (argv, env)
}

/// A process's environment, as `KEY=value` strings: as it was at exec, the
/// same as Linux's /proc/<pid>/environ.
pub fn environ(pid: i32) -> Vec<String> {
    procargs(pid)
        .map(|b| split_procargs(&b).1)
        .unwrap_or_default()
}

/// A process's command line.
pub fn cmdline(pid: i32) -> Vec<String> {
    procargs(pid)
        .map(|b| {
            split_procargs(&b)
                .0
                .into_iter()
                .filter(|a| !a.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// xnu bsd/sys/proc_info.h: `struct proc_fileinfo`, `struct
/// vnode_fdinfowithpath` and its flavor, which the libc crate doesn't have.
#[repr(C)]
struct ProcFileinfo {
    fi_openflags: u32,
    fi_status: u32,
    fi_offset: i64,
    fi_type: i32,
    fi_guardflags: u32,
}

#[repr(C)]
struct VnodeFdinfoWithPath {
    pfi: ProcFileinfo,
    pvip: libc::vnode_info_path,
}

const PROC_PIDFDVNODEPATHINFO: c_int = 2;

fn vip_path(p: &libc::vnode_info_path) -> Option<PathBuf> {
    let bytes: &[u8] =
        unsafe { std::slice::from_raw_parts(p.vip_path.as_ptr().cast(), size_of_val(&p.vip_path)) };
    let s = CStr::from_bytes_until_nul(bytes).ok()?.to_str().ok()?;
    (!s.is_empty()).then(|| PathBuf::from(s))
}

unsafe extern "C" {
    fn devname_r(dev: libc::dev_t, kind: libc::mode_t, buf: *mut c_char, len: c_int)
    -> *mut c_char;
}

/// What a process's standard input is (a terminal's device path, say), as
/// lsof reads another process's descriptors.
pub fn stdin(pid: i32) -> Option<PathBuf> {
    let mut info: VnodeFdinfoWithPath = unsafe { zeroed() };
    let size = size_of::<VnodeFdinfoWithPath>() as c_int;
    let n = unsafe {
        libc::proc_pidfdinfo(
            pid,
            0,
            PROC_PIDFDVNODEPATHINFO,
            (&raw mut info).cast::<c_void>(),
            size,
        )
    };
    if n != size {
        return None;
    }
    if let Some(p) = vip_path(&info.pvip) {
        return Some(p);
    }
    // A device with no path recorded: its name from the device number.
    let st = &info.pvip.vip_vi.vi_stat;
    if st.vst_mode & libc::S_IFMT != libc::S_IFCHR {
        return None;
    }
    let mut name = [0 as c_char; 64];
    let got = unsafe {
        devname_r(
            st.vst_rdev as libc::dev_t,
            libc::S_IFCHR,
            name.as_mut_ptr(),
            name.len() as c_int,
        )
    };
    if got.is_null() {
        return None;
    }
    let name = unsafe { CStr::from_ptr(got) }.to_str().ok()?;
    Some(PathBuf::from("/dev").join(name))
}

fn pids() -> Vec<i32> {
    let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if count < 1 {
        return Vec::new();
    }
    // Room for processes started between the two calls.
    let mut buf = vec![0i32; count as usize + 64];
    let got = unsafe {
        libc::proc_listallpids(
            buf.as_mut_ptr().cast(),
            (buf.len() * size_of::<i32>()) as c_int,
        )
    };
    buf.truncate(got.max(0) as usize);
    buf
}

/// Every process's current folder.
pub fn folders_in_use() -> Vec<PathBuf> {
    pids()
        .into_iter()
        .filter_map(|pid| unsafe {
            pidinfo::<libc::proc_vnodepathinfo>(pid, libc::PROC_PIDVNODEPATHINFO)
        })
        .filter_map(|v| vip_path(&v.pvi_cdir))
        .collect()
}

/// Changes at every boot: how toomux tells a restart from a lost tmux server.
/// A random id the kernel makes at each boot; `kern.boottime` would do
/// too, but it moves when the clock is set.
pub fn boot_id() -> String {
    let mut buf = [0u8; 64];
    let mut len = buf.len();
    let ok = unsafe {
        libc::sysctlbyname(
            c"kern.bootsessionuuid".as_ptr(),
            buf.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    } == 0;
    if !ok {
        return String::new();
    }
    CStr::from_bytes_until_nul(&buf[..len.min(buf.len())])
        .ok()
        .and_then(|s| s.to_str().ok())
        .unwrap_or_default()
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buf(argc: i32, strings: &[u8]) -> Vec<u8> {
        let mut b = argc.to_ne_bytes().to_vec();
        b.extend_from_slice(strings);
        b
    }

    #[test]
    fn procargs_split() {
        let b = buf(2, b"/usr/bin/claude\0\0\0\0claude\0--resume\0TMUX=/private/tmp/tmux-501/default,1,0\0TMUX_PANE=%3\0\0\0pfz=0x1\0ptr_munge=\0");
        let (argv, env) = split_procargs(&b);
        assert_eq!(argv, ["claude", "--resume"]);
        assert_eq!(
            env,
            ["TMUX=/private/tmp/tmux-501/default,1,0", "TMUX_PANE=%3"]
        );
    }

    #[test]
    fn procargs_env_ending_aligned() {
        let b = buf(1, b"/bin/x\0\0x\0A=1\0pfz=0x1\0stack_guard=0x2\0");
        assert_eq!(split_procargs(&b).1, ["A=1"]);
    }

    #[test]
    fn procargs_title_overwrote_argv() {
        // node's process.title: the title, then zeros over the rest of argv.
        let b = buf(3, b"/usr/local/bin/node\0\0node\0\0\0\0\0\0\0\0\0\0\0CLAUDE_CONFIG_DIR=/Users/x/.claude-work\0\0");
        let (argv, env) = split_procargs(&b);
        assert_eq!(argv, ["node", "", ""]);
        assert_eq!(env, ["CLAUDE_CONFIG_DIR=/Users/x/.claude-work"]);
    }

    #[test]
    fn procargs_env_hidden() {
        // The kernel leaves the environment out for a restricted process.
        let b = buf(1, b"/bin/x\0\0x\0");
        assert_eq!(split_procargs(&b), (vec!["x".to_string()], vec![]));
    }

    #[test]
    fn this_process() {
        let me = std::process::id() as i32;
        assert!(start_time(me).is_some());
        assert!(!is_zombie(me));
        assert!(environ(me).iter().any(|v| v.starts_with("PATH=")));
        assert!(!cmdline(me).is_empty());
        assert!(!folders_in_use().is_empty());
        assert_eq!(boot_id().len(), 36);
        assert_eq!(start_time(me), start_time(me));
    }

    #[test]
    fn zombie_is_gone_but_zombie() {
        let child = std::process::Command::new("/usr/bin/true").spawn().unwrap();
        let pid = child.id() as i32;
        // Unreaped: a zombie until waited for.
        for _ in 0..200 {
            if is_zombie(pid) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(is_zombie(pid));
        assert_eq!(start_time(pid), None);
        drop(child);
    }

    /// The macOS CI job runs this against a real `claude` in a tmux pane:
    /// TOOMUX_CHECK_PID is its pid, TOOMUX_CHECK_TTY that pane's tty.
    #[test]
    #[ignore]
    fn a_real_claude() {
        let pid: i32 = std::env::var("TOOMUX_CHECK_PID").unwrap().parse().unwrap();
        let tty = std::env::var("TOOMUX_CHECK_TTY").unwrap();
        let env = environ(pid);
        assert!(
            env.iter().any(|v| v.starts_with("CLAUDE_CONFIG_DIR=")),
            "no environment read from {pid}"
        );
        assert!(env.iter().any(|v| v.starts_with("TMUX_PANE=")));
        assert!(
            cmdline(pid).iter().any(|a| a.contains("claude")),
            "{:?}",
            cmdline(pid)
        );
        assert!(start_time(pid).is_some());
        assert_eq!(stdin(pid), Some(PathBuf::from(tty)));
    }
}
