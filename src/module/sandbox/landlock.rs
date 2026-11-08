//! Landlock domain for a module process.
//!
//! The ruleset is built in the parent. After fork the child only runs syscalls:
//! `PR_SET_NO_NEW_PRIVS`, `landlock_restrict_self`, a seccomp filter, then
//! `close_range`. Landlock handles TCP bind and connect and adds no port rule,
//! so those operations are denied. Landlock has no UDP right, so the seccomp
//! filter allows `socket` only for `AF_UNIX`. `close_range` drops inherited
//! sockets, including a UDP socket the node already holds. The filesystem is an
//! allow-list: the module data directory, the module binary, the IPC socket
//! directory, and the system paths a dynamic loader needs.

use std::fs::OpenOptions;
use std::io::{self, ErrorKind};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(all(target_os = "linux", feature = "libc"))]
const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1;
#[cfg(all(target_os = "linux", feature = "libc"))]
const LANDLOCK_RULE_PATH_BENEATH: u32 = 1;

#[cfg(all(target_os = "linux", feature = "libc"))]
const FS_EXECUTE: u64 = 1 << 0;
#[cfg(all(target_os = "linux", feature = "libc"))]
const FS_WRITE_FILE: u64 = 1 << 1;
#[cfg(all(target_os = "linux", feature = "libc"))]
const FS_READ_FILE: u64 = 1 << 2;
#[cfg(all(target_os = "linux", feature = "libc"))]
const FS_READ_DIR: u64 = 1 << 3;
#[cfg(all(target_os = "linux", feature = "libc"))]
const FS_REMOVE_DIR: u64 = 1 << 4;
#[cfg(all(target_os = "linux", feature = "libc"))]
const FS_REMOVE_FILE: u64 = 1 << 5;
#[cfg(all(target_os = "linux", feature = "libc"))]
const FS_MAKE_CHAR: u64 = 1 << 6;
#[cfg(all(target_os = "linux", feature = "libc"))]
const FS_MAKE_DIR: u64 = 1 << 7;
#[cfg(all(target_os = "linux", feature = "libc"))]
const FS_MAKE_REG: u64 = 1 << 8;
#[cfg(all(target_os = "linux", feature = "libc"))]
const FS_MAKE_SOCK: u64 = 1 << 9;
#[cfg(all(target_os = "linux", feature = "libc"))]
const FS_MAKE_FIFO: u64 = 1 << 10;
#[cfg(all(target_os = "linux", feature = "libc"))]
const FS_MAKE_BLOCK: u64 = 1 << 11;
#[cfg(all(target_os = "linux", feature = "libc"))]
const FS_MAKE_SYM: u64 = 1 << 12;
#[cfg(all(target_os = "linux", feature = "libc"))]
const FS_REFER: u64 = 1 << 13;
#[cfg(all(target_os = "linux", feature = "libc"))]
const FS_TRUNCATE: u64 = 1 << 14;
#[cfg(all(target_os = "linux", feature = "libc"))]
const FS_IOCTL_DEV: u64 = 1 << 15;

#[cfg(all(target_os = "linux", feature = "libc"))]
const NET_BIND_TCP: u64 = 1 << 0;
#[cfg(all(target_os = "linux", feature = "libc"))]
const NET_CONNECT_TCP: u64 = 1 << 1;

#[cfg(all(target_os = "linux", feature = "libc"))]
const SCOPE_ABSTRACT_UNIX_SOCKET: u64 = 1 << 0;
#[cfg(all(target_os = "linux", feature = "libc"))]
const SCOPE_SIGNAL: u64 = 1 << 1;

#[cfg(all(target_os = "linux", feature = "libc"))]
const RO_DIRS: &[&str] = &["/usr", "/lib", "/lib64", "/etc"];
#[cfg(all(target_os = "linux", feature = "libc"))]
const RO_FILES: &[&str] = &["/dev/urandom", "/dev/zero"];

/// Ruleset fd. Dropping it closes the fd. It must stay open until the child
/// has called `landlock_restrict_self`.
pub struct ModuleLandlock {
    fd: OwnedFd,
}

#[cfg(all(target_os = "linux", feature = "libc"))]
#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
    handled_access_net: u64,
    scoped: u64,
}

#[cfg(all(target_os = "linux", feature = "libc"))]
#[repr(C, packed)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

impl ModuleLandlock {
    /// Build a ruleset for one module. Fails when Landlock cannot deny TCP.
    ///
    /// `modules_dir` is the directory that holds `.master_key`. It is never
    /// granted, including when the module binary lives directly inside it.
    pub fn prepare(
        binary: &Path,
        data_dir: &Path,
        socket_path: &Path,
        modules_dir: &Path,
    ) -> io::Result<Self> {
        #[cfg(not(all(target_os = "linux", feature = "libc")))]
        {
            let _ = (binary, data_dir, socket_path, modules_dir);
            return Err(io::Error::new(
                ErrorKind::Unsupported,
                "refusing to spawn a module without Landlock",
            ));
        }
        #[cfg(all(target_os = "linux", feature = "libc"))]
        {
            prepare_linux(binary, data_dir, socket_path, modules_dir)
        }
    }

    pub fn raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// Install the domain on a Tokio child. The ruleset fd must outlive `spawn`.
    pub fn arm_tokio(&self, command: &mut tokio::process::Command) {
        #[cfg(unix)]
        {
            let fd = self.raw_fd();
            // SAFETY: the child runs only `landlock_restrict_self`. No allocation.
            unsafe {
                command.pre_exec(move || restrict_fd(fd));
            }
        }
        #[cfg(not(unix))]
        {
            let _ = command;
        }
    }

    /// Same domain for a std child. Used by the sandbox test.
    #[cfg(unix)]
    pub fn arm_std(&self, command: &mut Command) {
        use std::os::unix::process::CommandExt;
        let fd = self.raw_fd();
        // SAFETY: the child runs only `landlock_restrict_self`. No allocation.
        unsafe {
            command.pre_exec(move || restrict_fd(fd));
        }
    }
}

/// Enact `fd` on the calling thread. Safe to run after `fork`.
pub fn restrict_fd(fd: RawFd) -> io::Result<()> {
    #[cfg(all(target_os = "linux", feature = "libc"))]
    {
        let rc = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        let rc = unsafe { libc::syscall(libc::SYS_landlock_restrict_self, fd, 0) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        install_unix_only_sockets()?;
        // Drop inherited sockets after the Landlock fd has been used.
        let rc = unsafe { libc::close_range(3, u32::MAX, 0) };
        if rc == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
    #[cfg(not(all(target_os = "linux", feature = "libc")))]
    {
        let _ = fd;
        Err(io::Error::new(
            ErrorKind::Unsupported,
            "Landlock requires Linux",
        ))
    }
}

#[cfg(all(target_os = "linux", feature = "libc"))]
fn prepare_linux(
    binary: &Path,
    data_dir: &Path,
    socket_path: &Path,
    modules_dir: &Path,
) -> io::Result<ModuleLandlock> {
    let abi = landlock_abi()?;
    if abi < 4 {
        return Err(io::Error::new(
            ErrorKind::Unsupported,
            format!("Landlock ABI {abi} cannot deny TCP; refusing to spawn"),
        ));
    }
    let fs = fs_mask(abi);
    let net = NET_BIND_TCP | NET_CONNECT_TCP;
    let scoped = if abi >= 6 {
        SCOPE_ABSTRACT_UNIX_SOCKET | SCOPE_SIGNAL
    } else {
        0
    };
    let fd = create_ruleset(fs, net, scoped)?;
    let ruleset = ModuleLandlock { fd };

    let ro = FS_READ_FILE | FS_READ_DIR | FS_EXECUTE;
    for path in RO_DIRS {
        add_if_exists(&ruleset, Path::new(path), ro)?;
    }
    // GPU builds link libcudart from here. The loader has to read it.
    add_if_exists(&ruleset, Path::new("/opt/cuda"), ro)?;
    add_if_exists(&ruleset, Path::new("/opt/cuda/lib64"), ro)?;
    for path in RO_FILES {
        add_if_exists(&ruleset, Path::new(path), FS_READ_FILE)?;
    }
    add_if_exists(
        &ruleset,
        Path::new("/dev/null"),
        FS_READ_FILE | FS_WRITE_FILE | FS_TRUNCATE,
    )?;

    let data_dir = data_dir.canonicalize()?;
    let modules_dir = modules_dir
        .canonicalize()
        .unwrap_or_else(|_| modules_dir.to_path_buf());
    if data_dir == Path::new("/") || data_dir == modules_dir || modules_dir.starts_with(&data_dir) {
        return Err(io::Error::new(
            ErrorKind::PermissionDenied,
            "module data directory would expose the modules directory",
        ));
    }
    add_path(&ruleset, &data_dir, fs)?;

    let binary = binary.canonicalize()?;
    add_path(&ruleset, &binary, FS_READ_FILE | FS_EXECUTE)?;
    if let Some(parent) = binary.parent() {
        let parent = parent
            .canonicalize()
            .unwrap_or_else(|_| parent.to_path_buf());
        if parent != Path::new("/") && parent != modules_dir && !modules_dir.starts_with(&parent) {
            add_path(&ruleset, &parent, ro)?;
        }
    }

    let socket_dir = socket_parent(socket_path)?;
    add_path(
        &ruleset,
        &socket_dir,
        FS_READ_FILE | FS_WRITE_FILE | FS_READ_DIR,
    )?;

    Ok(ruleset)
}

#[cfg(all(target_os = "linux", feature = "libc"))]
fn install_unix_only_sockets() -> io::Result<()> {
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        return Err(io::Error::new(
            ErrorKind::Unsupported,
            "module socket filter is implemented for x86_64 and aarch64",
        ));
    }
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    {
        #[cfg(target_arch = "x86_64")]
        const AUDIT_ARCH: u32 = 0xC000_003E;
        #[cfg(target_arch = "aarch64")]
        const AUDIT_ARCH: u32 = 0xC000_00B7;
        const LD: u16 = 0x20;
        const JEQ: u16 = 0x15;
        const RET: u16 = 0x06;
        const ALLOW: u32 = libc::SECCOMP_RET_ALLOW as u32;
        const DENY: u32 = (libc::SECCOMP_RET_ERRNO as u32) | (libc::EPERM as u32);
        // arch, nr, then domain. Non-socket syscalls are allowed. socket() is
        // allowed only for AF_UNIX, which is what the module IPC uses.
        let filter = [
            libc::sock_filter { code: LD, jt: 0, jf: 0, k: 4 },
            libc::sock_filter { code: JEQ, jt: 0, jf: 5, k: AUDIT_ARCH },
            libc::sock_filter { code: LD, jt: 0, jf: 0, k: 0 },
            libc::sock_filter {
                code: JEQ,
                jt: 0,
                jf: 2,
                k: libc::SYS_socket as u32,
            },
            libc::sock_filter { code: LD, jt: 0, jf: 0, k: 16 },
            libc::sock_filter {
                code: JEQ,
                jt: 0,
                jf: 1,
                k: libc::AF_UNIX as u32,
            },
            libc::sock_filter { code: RET, jt: 0, jf: 0, k: ALLOW },
            libc::sock_filter { code: RET, jt: 0, jf: 0, k: DENY },
        ];
        let mut prog = libc::sock_fprog {
            len: filter.len() as libc::c_ushort,
            filter: filter.as_ptr() as *mut libc::sock_filter,
        };
        let rc = unsafe {
            libc::syscall(
                libc::SYS_seccomp,
                libc::SECCOMP_SET_MODE_FILTER,
                0,
                &mut prog,
            )
        };
        if rc == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

#[cfg(all(target_os = "linux", feature = "libc"))]
fn fs_mask(abi: i64) -> u64 {
    let mut mask = FS_EXECUTE
        | FS_WRITE_FILE
        | FS_READ_FILE
        | FS_READ_DIR
        | FS_REMOVE_DIR
        | FS_REMOVE_FILE
        | FS_MAKE_CHAR
        | FS_MAKE_DIR
        | FS_MAKE_REG
        | FS_MAKE_SOCK
        | FS_MAKE_FIFO
        | FS_MAKE_BLOCK
        | FS_MAKE_SYM;
    if abi >= 2 {
        mask |= FS_REFER;
    }
    if abi >= 3 {
        mask |= FS_TRUNCATE;
    }
    if abi >= 5 {
        mask |= FS_IOCTL_DEV;
    }
    mask
}

#[cfg(all(target_os = "linux", feature = "libc"))]
fn landlock_abi() -> io::Result<i64> {
    let ver = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<u8>(),
            0usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    if ver <= 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(ver)
}

#[cfg(all(target_os = "linux", feature = "libc"))]
fn create_ruleset(fs: u64, net: u64, scoped: u64) -> io::Result<OwnedFd> {
    let attr = RulesetAttr {
        handled_access_fs: fs,
        handled_access_net: net,
        scoped,
    };
    let fd = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            &attr as *const RulesetAttr,
            std::mem::size_of::<RulesetAttr>(),
            0,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = fd as RawFd;
    let _ = unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

#[cfg(all(target_os = "linux", feature = "libc"))]
fn add_if_exists(ruleset: &ModuleLandlock, path: &Path, access: u64) -> io::Result<()> {
    if path.exists() {
        add_path(ruleset, path, access)?;
    }
    Ok(())
}

#[cfg(all(target_os = "linux", feature = "libc"))]
fn add_path(ruleset: &ModuleLandlock, path: &Path, access: u64) -> io::Result<()> {
    let file = OpenOptions::new().read(true).open(path)?;
    let attr = PathBeneathAttr {
        allowed_access: access,
        parent_fd: file.as_raw_fd(),
    };
    let rc = unsafe {
        libc::syscall(
            libc::SYS_landlock_add_rule,
            ruleset.raw_fd(),
            LANDLOCK_RULE_PATH_BENEATH,
            &attr as *const PathBeneathAttr,
            0,
        )
    };
    if rc != 0 {
        return Err(io::Error::new(
            io::Error::last_os_error().kind(),
            format!(
                "landlock rule for {}: {}",
                path.display(),
                io::Error::last_os_error()
            ),
        ));
    }
    Ok(())
}

#[cfg(all(target_os = "linux", feature = "libc"))]
fn socket_parent(socket_path: &Path) -> io::Result<PathBuf> {
    let candidate = if socket_path.exists() {
        socket_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(socket_path)
    } else if let Some(parent) = socket_path.parent().filter(|p| p.exists()) {
        parent
    } else {
        return Err(io::Error::new(
            ErrorKind::NotFound,
            format!(
                "IPC socket directory does not exist: {}",
                socket_path.display()
            ),
        ));
    };
    candidate.canonicalize()
}

#[cfg(all(test, target_os = "linux", feature = "libc"))]
mod tests {
    use super::*;

    #[test]
    fn child_reads_its_data_dir_and_cannot_read_outside_or_connect_tcp() {
        let root = std::env::temp_dir().join(format!(
            "blvm-landlock-{}-{}",
            std::process::id(),
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let data = root.join("data");
        let socket_dir = root.join("sock");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&socket_dir).unwrap();
        std::fs::write(data.join("ok.txt"), b"hello").unwrap();
        let secret = root.join("secret.txt");
        std::fs::write(&secret, b"secret").unwrap();

        let ruleset = ModuleLandlock::prepare(
            Path::new("/bin/cat"),
            &data,
            &socket_dir.join("node.sock"),
            &root,
        )
        .expect("landlock ruleset");

        let mut allow = Command::new("/bin/cat");
        allow.arg(data.join("ok.txt"));
        ruleset.arm_std(&mut allow);
        let allowed = allow.output().expect("spawn cat");
        assert!(
            allowed.status.success(),
            "cat of the data dir failed: {}",
            String::from_utf8_lossy(&allowed.stderr)
        );
        assert_eq!(allowed.stdout, b"hello");

        let mut deny = Command::new("/bin/cat");
        deny.arg(&secret);
        ruleset.arm_std(&mut deny);
        let denied = deny.output().expect("spawn cat");
        assert!(
            !denied.status.success(),
            "cat of a file outside the data dir succeeded"
        );

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let python = Path::new("/usr/bin/python3");
        if python.exists() {
            let mut tcp = Command::new(python);
            tcp.args([
                "-c",
                "import socket,sys\n\
                 try:\n\
                 \tsocket.create_connection((sys.argv[1], int(sys.argv[2])), 2)\n\
                 except PermissionError:\n\
                 \tsys.exit(13)\n\
                 except OSError as e:\n\
                 \tsys.stderr.write(f'oserror {e.errno}\\n')\n\
                 \tsys.exit(2)\n\
                 sys.exit(0)\n",
                "127.0.0.1",
                &port.to_string(),
            ]);
            ruleset.arm_std(&mut tcp);
            let out = tcp.output().expect("spawn python");
            let err = String::from_utf8_lossy(&out.stderr);
            assert_eq!(
                out.status.code(),
                Some(13),
                "expected permission denied from TCP connect, stderr: {err}"
            );

            let mut udp = Command::new(python);
            udp.args([
                "-c",
                "import socket,sys\n\
                 try:\n\
                 \ts=socket.socket(socket.AF_INET, socket.SOCK_DGRAM)\n\
                 \ts.sendto(b'x', ('127.0.0.1', 9))\n\
                 except PermissionError:\n\
                 \tsys.exit(13)\n\
                 except OSError as e:\n\
                 \tsys.stderr.write(f'oserror {e.errno}\\n')\n\
                 \tsys.exit(2)\n\
                 sys.exit(0)\n",
            ]);
            ruleset.arm_std(&mut udp);
            let out = udp.output().expect("spawn python udp");
            let err = String::from_utf8_lossy(&out.stderr);
            assert_eq!(
                out.status.code(),
                Some(13),
                "expected permission denied from UDP, stderr: {err}"
            );

            let ipc = socket_dir.join("node.sock");
            let _listener = std::os::unix::net::UnixListener::bind(&ipc).expect("unix listener");
            let mut unix = Command::new(python);
            unix.args([
                "-c",
                "import socket,sys\n\
                 s=socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)\n\
                 s.connect(sys.argv[1])\n\
                 sys.exit(0)\n",
                &ipc.to_string_lossy(),
            ]);
            ruleset.arm_std(&mut unix);
            let out = unix.output().expect("spawn python unix");
            let err = String::from_utf8_lossy(&out.stderr);
            assert!(
                out.status.success(),
                "AF_UNIX connect failed: {err}"
            );
        }
        drop(listener);
        let _ = std::fs::remove_dir_all(&root);
    }
}
