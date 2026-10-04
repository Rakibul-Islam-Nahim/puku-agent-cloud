//! Everything that only exists on Linux: init duties and the vsock server.

use std::collections::BTreeMap;
use std::ffi::CString;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Command, Stdio};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use puku_cloud_proto::guest_proto::{kind, GuestReply, GuestRequest, GUEST_AGENT_PORT};

use crate::cmdline::{self, Boot};
use crate::frames::{read_frame, write_frame, write_reply};

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn log(msg: impl AsRef<str>) {
    // stderr is the VM console, which Cloud Hypervisor writes to a file.
    eprintln!("puku-guestd: {}", msg.as_ref());
}

pub fn main() {
    // Test and development mode: serve on TCP, do no init duties. This is how
    // the protocol is exercised in an ordinary Linux container, no VM needed.
    if let Ok(listen) = std::env::var("PUKU_GUESTD_LISTEN") {
        if let Some(addr) = listen.strip_prefix("tcp:") {
            serve_tcp(addr);
        }
    }
    if unsafe { libc::getpid() } != 1 {
        log("not PID 1 and PUKU_GUESTD_LISTEN is not tcp:<addr>; nothing to do");
        std::process::exit(2);
    }
    let boot = match setup_system() {
        Ok(b) => b,
        Err(e) => {
            // Keep going: a guest with a half-set-up system that still answers
            // on vsock can be diagnosed; one whose init died cannot.
            log(format!("system setup failed: {e}"));
            Boot::default()
        }
    };
    supervise(boot)
}

// ------------------------------------------------------------------ init

fn cstr(s: &str) -> CString {
    CString::new(s).expect("no NUL in a path")
}

fn mount(src: &str, target: &str, fstype: &str, flags: libc::c_ulong, data: Option<&str>) -> io::Result<()> {
    let _ = std::fs::create_dir_all(target);
    let data = data.map(cstr);
    let rc = unsafe {
        libc::mount(
            cstr(src).as_ptr(),
            cstr(target).as_ptr(),
            cstr(fstype).as_ptr(),
            flags,
            data.as_ref().map_or(std::ptr::null(), |d| d.as_ptr() as *const libc::c_void),
        )
    };
    if rc == 0 {
        return Ok(());
    }
    let e = io::Error::last_os_error();
    if e.raw_os_error() == Some(libc::EBUSY) {
        return Ok(()); // already mounted
    }
    Err(io::Error::new(e.kind(), format!("mounting {fstype} on {target}: {e}")))
}

/// The pseudo-filesystems every Linux userland assumes, under `root`.
fn mount_basics(root: &str, shm: &str) -> io::Result<()> {
    let nosuid_nodev = libc::MS_NOSUID | libc::MS_NODEV;
    mount("proc", &format!("{root}/proc"), "proc", nosuid_nodev | libc::MS_NOEXEC, None)?;
    mount("sysfs", &format!("{root}/sys"), "sysfs", nosuid_nodev | libc::MS_NOEXEC, None)?;
    mount("devtmpfs", &format!("{root}/dev"), "devtmpfs", libc::MS_NOSUID, Some("mode=0755"))?;
    mount("devpts", &format!("{root}/dev/pts"), "devpts", libc::MS_NOSUID | libc::MS_NOEXEC, Some("gid=5,mode=620,ptmxmode=666"))?;
    mount("tmpfs", &format!("{root}/dev/shm"), "tmpfs", nosuid_nodev, Some(&format!("mode=1777,size={shm}")))?;
    mount("tmpfs", &format!("{root}/run"), "tmpfs", nosuid_nodev, Some("mode=0755"))?;
    mount("tmpfs", &format!("{root}/tmp"), "tmpfs", nosuid_nodev, Some("mode=1777"))?;
    Ok(())
}

/// Lay the system out: an overlay root (read-only image under a per-VM
/// writable disk), the basics, the host's shares, hostname and network.
fn setup_system() -> io::Result<Boot> {
    // /proc first: the command line is in it.
    mount("proc", "/proc", "proc", 0, None)?;
    let boot = cmdline::parse(&std::fs::read_to_string("/proc/cmdline").unwrap_or_default());
    let shm = boot.shm.clone().unwrap_or_else(|| "256m".into());

    if let Some(dev) = &boot.overlay {
        if let Err(e) = switch_to_overlay(dev) {
            log(format!("overlay root unavailable ({e}); running on the read-only image"));
        }
    }
    mount_basics("", &shm)?;

    for share in &boot.shares {
        std::fs::create_dir_all(&share.path)?;
        if let Err(e) = mount(&share.tag, &share.path, "virtiofs", 0, None) {
            log(format!("share {} -> {}: {e}", share.tag, share.path));
        }
    }
    if let Some(name) = &boot.hostname {
        unsafe { libc::sethostname(name.as_ptr() as *const libc::c_char, name.len()) };
        let _ = std::fs::write("/etc/hostname", format!("{name}\n"));
    }
    if let Err(e) = configure_network(&boot) {
        log(format!("network setup: {e}"));
    }
    Ok(boot)
}

/// Stop everything the way an init should before the power goes: ask every
/// process to exit and give them a moment to close their files -- a
/// browser's SQLite databases above all, which the snapshot taken after this
/// stop captures -- then flush and power off.
fn power_off() -> ! {
    // From PID 1, kill(-1) reaches every process but init itself.
    unsafe { libc::kill(-1, libc::SIGTERM) };
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    // kill(-1, 0) fails with ESRCH once there is nobody left to signal.
    while std::time::Instant::now() < deadline && unsafe { libc::kill(-1, 0) } == 0 {
        std::thread::sleep(Duration::from_millis(100));
    }
    unsafe {
        libc::kill(-1, libc::SIGKILL);
        libc::sync();
        libc::reboot(libc::RB_POWER_OFF);
    }
    // reboot(2) does not return for PID 1; if it somehow did, stop here.
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

/// Build `lowerdir=/ upperdir=<disk>` and pivot into it, so the image stays
/// pristine and shared between VMs while each VM writes to its own disk.
fn switch_to_overlay(dev: &str) -> io::Result<()> {
    // /mnt exists in every mainstream image; a tmpfs there is scratch space
    // on an otherwise read-only root.
    mount("tmpfs", "/mnt", "tmpfs", 0, Some("mode=0755"))?;
    // discard: blocks the guest frees are punched out of the host's sparse
    // file, so a kept root disk -- and every snapshot of it -- stays the size
    // of what is on it, not of everything ever written.
    mount(dev, "/mnt/upper", "ext4", libc::MS_NOATIME, Some("discard"))?;
    std::fs::create_dir_all("/mnt/upper/data")?;
    std::fs::create_dir_all("/mnt/upper/work")?;
    mount(
        "overlay",
        "/mnt/root",
        "overlay",
        0,
        Some("lowerdir=/,upperdir=/mnt/upper/data,workdir=/mnt/upper/work"),
    )?;
    std::fs::create_dir_all("/mnt/root/.oldroot")?;
    let rc = unsafe {
        libc::syscall(
            libc::SYS_pivot_root,
            cstr("/mnt/root").as_ptr(),
            cstr("/mnt/root/.oldroot").as_ptr(),
        )
    };
    if rc != 0 {
        return Err(io::Error::other(format!("pivot_root: {}", io::Error::last_os_error())));
    }
    std::env::set_current_dir("/")?;
    // The old root's mounts go; the overlay keeps its own references to the
    // layers it needs.
    unsafe { libc::umount2(cstr("/.oldroot").as_ptr(), libc::MNT_DETACH) };
    let _ = std::fs::remove_dir("/.oldroot");
    Ok(())
}

fn sockaddr_in(octets: [u8; 4]) -> libc::sockaddr {
    let sin = libc::sockaddr_in {
        sin_family: libc::AF_INET as libc::sa_family_t,
        sin_port: 0,
        sin_addr: libc::in_addr { s_addr: u32::from_ne_bytes(octets) },
        sin_zero: [0; 8],
    };
    unsafe { std::mem::transmute::<libc::sockaddr_in, libc::sockaddr>(sin) }
}

fn ifreq(name: &str) -> libc::ifreq {
    let mut ifr: libc::ifreq = unsafe { std::mem::zeroed() };
    for (dst, src) in ifr.ifr_name.iter_mut().zip(name.bytes()) {
        *dst = src as libc::c_char;
    }
    ifr
}

fn ioctl<T>(fd: i32, req: libc::c_ulong, arg: &mut T, what: &str) -> io::Result<()> {
    if unsafe { libc::ioctl(fd, req as _, arg as *mut T) } < 0 {
        return Err(io::Error::other(format!("{what}: {}", io::Error::last_os_error())));
    }
    Ok(())
}

fn link_up(fd: i32, name: &str) -> io::Result<()> {
    let mut ifr = ifreq(name);
    ioctl(fd, libc::SIOCGIFFLAGS, &mut ifr, "reading interface flags")?;
    unsafe { ifr.ifr_ifru.ifru_flags |= (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_short };
    ioctl(fd, libc::SIOCSIFFLAGS, &mut ifr, "raising the interface")
}

/// Loopback up; eth0 addressed and up; default route; resolver. With plain
/// ioctls, because guest images do not reliably ship `ip`.
fn configure_network(boot: &Boot) -> io::Result<()> {
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let raw = fd.as_raw_fd();
    link_up(raw, "lo")?;
    let Some((addr, mask)) = boot.ip.as_deref().and_then(cmdline::parse_cidr) else {
        return Ok(()); // no network for this VM
    };
    let mut ifr = ifreq("eth0");
    ifr.ifr_ifru.ifru_addr = sockaddr_in(addr);
    ioctl(raw, libc::SIOCSIFADDR, &mut ifr, "setting the address")?;
    let mut ifr = ifreq("eth0");
    ifr.ifr_ifru.ifru_netmask = sockaddr_in(mask);
    ioctl(raw, libc::SIOCSIFNETMASK, &mut ifr, "setting the netmask")?;
    link_up(raw, "eth0")?;
    if let Some(gw) = boot.gateway.as_deref().and_then(|g| g.parse::<std::net::Ipv4Addr>().ok()) {
        let mut rt: libc::rtentry = unsafe { std::mem::zeroed() };
        rt.rt_dst = sockaddr_in([0; 4]);
        rt.rt_genmask = sockaddr_in([0; 4]);
        rt.rt_gateway = sockaddr_in(gw.octets());
        rt.rt_flags = libc::RTF_UP | libc::RTF_GATEWAY;
        ioctl(raw, libc::SIOCADDRT, &mut rt, "adding the default route")?;
    }
    if let Some(dns) = &boot.dns {
        let _ = std::fs::write("/etc/resolv.conf", format!("nameserver {dns}\n"));
    }
    Ok(())
}

/// PID 1 forever: run the server as a child, reap everything, and restart
/// the server if it dies. Orphans (a runner daemonized with setsid) are
/// reparented here, so without the reaping they would pile up as zombies.
fn supervise(boot: Boot) -> ! {
    let _ = boot;
    loop {
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            serve_vsock();
            std::process::exit(0);
        }
        if pid < 0 {
            log(format!("fork: {}", io::Error::last_os_error()));
            std::thread::sleep(Duration::from_secs(1));
            continue;
        }
        loop {
            let mut status = 0;
            let r = unsafe { libc::waitpid(-1, &mut status, 0) };
            if r == pid {
                log("server exited; restarting it");
                break;
            }
            if r < 0 {
                std::thread::sleep(Duration::from_millis(200));
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

// ---------------------------------------------------------------- server

/// A connected stream: vsock or, in test mode, TCP. Both are plain fds.
struct Conn {
    file: std::fs::File,
}

impl Conn {
    fn from_fd(fd: i32) -> Conn {
        Conn { file: unsafe { std::fs::File::from_raw_fd(fd) } }
    }
    fn try_clone(&self) -> io::Result<Conn> {
        Ok(Conn { file: self.file.try_clone()? })
    }
    fn shutdown_write(&self) {
        unsafe { libc::shutdown(self.file.as_raw_fd(), libc::SHUT_WR) };
    }
}

impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.file.read(buf)
    }
}
impl Write for Conn {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.file.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn serve_vsock() {
    let fd = unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        log(format!("vsock socket: {}", io::Error::last_os_error()));
        return;
    }
    let mut addr: libc::sockaddr_vm = unsafe { std::mem::zeroed() };
    addr.svm_family = libc::AF_VSOCK as libc::sa_family_t;
    addr.svm_port = GUEST_AGENT_PORT;
    addr.svm_cid = libc::VMADDR_CID_ANY;
    let rc = unsafe {
        libc::bind(
            fd,
            &addr as *const libc::sockaddr_vm as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t,
        )
    };
    if rc != 0 || unsafe { libc::listen(fd, 64) } != 0 {
        log(format!("vsock bind/listen: {}", io::Error::last_os_error()));
        return;
    }
    log(format!("listening on vsock port {GUEST_AGENT_PORT}"));
    loop {
        let c = unsafe { libc::accept4(fd, std::ptr::null_mut(), std::ptr::null_mut(), libc::SOCK_CLOEXEC) };
        if c < 0 {
            continue;
        }
        let conn = Conn::from_fd(c);
        std::thread::spawn(move || handle(conn));
    }
}

fn serve_tcp(addr: &str) -> ! {
    let listener = std::net::TcpListener::bind(addr).expect("binding the test listener");
    log(format!("test mode: listening on tcp {addr}"));
    for stream in listener.incoming().flatten() {
        let conn = Conn::from_fd(stream.into_raw_fd());
        std::thread::spawn(move || handle(conn));
    }
    std::process::exit(0)
}

/// Environment and hostname from `Init`, plus the image's own ENV.
#[derive(Default, Clone)]
struct Base {
    env: BTreeMap<String, String>,
    workdir: Option<String>,
}

fn base() -> &'static Mutex<Base> {
    static BASE: OnceLock<Mutex<Base>> = OnceLock::new();
    BASE.get_or_init(|| Mutex::new(image_config()))
}

/// What `docker export` loses and build-ch-rootfs.sh saves: the image's ENV
/// and WORKDIR, from `docker inspect`.
fn image_config() -> Base {
    let mut b = Base::default();
    b.env.insert("PATH".into(), "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into());
    if let Ok(bytes) = std::fs::read("/etc/puku/image-config.json") {
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) {
            for kv in v["Env"].as_array().into_iter().flatten().filter_map(|e| e.as_str()) {
                if let Some((k, val)) = kv.split_once('=') {
                    b.env.insert(k.into(), val.into());
                }
            }
            b.workdir = v["WorkingDir"].as_str().filter(|w| !w.is_empty()).map(String::from);
        }
    }
    b
}

fn handle(mut conn: Conn) {
    let req = match read_frame(&mut conn) {
        Ok(Some((kind::JSON, payload))) => match serde_json::from_slice::<GuestRequest>(&payload) {
            Ok(r) => r,
            Err(e) => {
                let _ = write_reply(&mut conn, &GuestReply::Error { message: format!("bad request: {e}") });
                return;
            }
        },
        _ => return,
    };
    let result = match req {
        GuestRequest::Ping => write_reply(&mut conn, &GuestReply::Pong { version: VERSION.into() }),
        GuestRequest::Init { env, hostname } => {
            base().lock().unwrap().env.extend(env);
            if !hostname.is_empty() {
                unsafe { libc::sethostname(hostname.as_ptr() as *const libc::c_char, hostname.len()) };
            }
            write_reply(&mut conn, &GuestReply::Ok)
        }
        GuestRequest::Exec { argv, env, cwd, user, timeout_ms, stdin } => {
            exec(conn, argv, env, cwd, user, timeout_ms.map(Duration::from_millis), stdin)
        }
        GuestRequest::Connect { port } => connect(conn, port),
        GuestRequest::Shutdown => {
            let _ = write_reply(&mut conn, &GuestReply::Ok);
            power_off()
        }
    };
    if let Err(e) = result {
        log(format!("request failed: {e}"));
    }
}

/// uid, gid and home for `user`: a number, `uid:gid`, or a passwd name.
fn resolve_user(user: &str) -> Option<(u32, u32, Option<String>)> {
    let passwd = std::fs::read_to_string("/etc/passwd").unwrap_or_default();
    let lookup = |pred: &dyn Fn(&[&str]) -> bool| {
        passwd.lines().map(|l| l.split(':').collect::<Vec<_>>()).find(|f| f.len() >= 6 && pred(f))
    };
    if let Some((u, g)) = user.split_once(':') {
        return Some((u.parse().ok()?, g.parse().ok()?, None));
    }
    if let Ok(uid) = user.parse::<u32>() {
        let entry = lookup(&|f| f[2] == user);
        let gid = entry.as_ref().and_then(|f| f[3].parse().ok()).unwrap_or(uid);
        return Some((uid, gid, entry.map(|f| f[5].to_string())));
    }
    let f = lookup(&|f| f[0] == user)?;
    Some((f[2].parse().ok()?, f[3].parse().ok()?, Some(f[5].to_string())))
}

fn pump(mut from: impl Read + Send + 'static, frame_kind: u8, to: Arc<Mutex<Conn>>, done: mpsc::Sender<()>) {
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match from.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if write_frame(&mut *to.lock().unwrap(), frame_kind, &buf[..n]).is_err() {
                        break;
                    }
                }
            }
        }
        let _ = done.send(());
    });
}

fn exec(
    conn: Conn,
    argv: Vec<String>,
    env: BTreeMap<String, String>,
    cwd: Option<String>,
    user: Option<String>,
    timeout: Option<Duration>,
    stdin: bool,
) -> io::Result<()> {
    let mut reader = conn.try_clone()?;
    let writer = Arc::new(Mutex::new(conn));
    let reply = |r: GuestReply| write_reply(&mut *writer.lock().unwrap(), &r);
    let Some((prog, args)) = argv.split_first() else {
        return reply(GuestReply::Error { message: "argv is empty".into() });
    };
    let base = base().lock().unwrap().clone();
    let ids = match user.as_deref().filter(|u| !u.is_empty() && *u != "root" && *u != "0") {
        None => None,
        Some(u) => match resolve_user(u) {
            Some(ids) => Some(ids),
            None => return reply(GuestReply::Error { message: format!("unknown user {u:?}") }),
        },
    };
    let mut full_env = base.env.clone();
    if let Some((_, _, Some(home))) = &ids {
        full_env.entry("HOME".into()).or_insert_with(|| home.clone());
    }
    full_env.entry("HOME".into()).or_insert_with(|| "/root".into());
    full_env.extend(env);

    let mut cmd = Command::new(prog);
    cmd.args(args)
        .env_clear()
        .envs(&full_env)
        .current_dir(cwd.or(base.workdir).unwrap_or_else(|| "/".into()))
        .stdin(if stdin { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let uid_gid = ids.map(|(u, g, _)| (u, g));
    unsafe {
        cmd.pre_exec(move || {
            // Its own process group, so a timeout can kill the command and
            // everything it spawned in one signal.
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            if let Some((uid, gid)) = uid_gid {
                if libc::setgroups(0, std::ptr::null()) != 0
                    || libc::setgid(gid) != 0
                    || libc::setuid(uid) != 0
                {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            // The shell convention for "could not run it at all".
            let _ = write_frame(&mut *writer.lock().unwrap(), kind::STDERR, format!("{prog}: {e}\n").as_bytes());
            return reply(GuestReply::Exited { code: if e.kind() == io::ErrorKind::NotFound { 127 } else { 126 }, timed_out: false });
        }
    };
    if let Some(mut sink) = child.stdin.take() {
        std::thread::spawn(move || {
            while let Ok(Some((k, data))) = read_frame(&mut reader) {
                match k {
                    kind::STDIN => {
                        if sink.write_all(&data).is_err() {
                            break;
                        }
                    }
                    kind::STDIN_EOF => break,
                    _ => {}
                }
            }
        });
    }
    let (done_tx, done_rx) = mpsc::channel();
    pump(child.stdout.take().unwrap(), kind::STDOUT, writer.clone(), done_tx.clone());
    pump(child.stderr.take().unwrap(), kind::STDERR, writer.clone(), done_tx);

    let pid = child.id() as i32;
    let deadline = timeout.map(|t| Instant::now() + t);
    let (status, timed_out) = loop {
        if let Some(st) = child.try_wait()? {
            break (st, false);
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            unsafe { libc::kill(-pid, libc::SIGKILL) };
            break (child.wait()?, true);
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    // Let the pumps drain, but not for ever: a daemon the command left
    // behind may hold its stdout open indefinitely.
    let drain_by = Instant::now() + Duration::from_secs(2);
    for _ in 0..2 {
        let left = drain_by.saturating_duration_since(Instant::now());
        if done_rx.recv_timeout(left).is_err() {
            break;
        }
    }
    let code = if timed_out {
        let _ = write_frame(
            &mut *writer.lock().unwrap(),
            kind::STDERR,
            format!("command timed out after {} ms\n", timeout.unwrap_or_default().as_millis()).as_bytes(),
        );
        124
    } else {
        status.code().unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
    };
    reply(GuestReply::Exited { code, timed_out })
}

fn connect(mut conn: Conn, port: u16) -> io::Result<()> {
    let tcp = match std::net::TcpStream::connect(("127.0.0.1", port)) {
        Ok(s) => s,
        Err(e) => {
            return write_reply(&mut conn, &GuestReply::Error { message: format!("nothing is listening on port {port}: {e}") })
        }
    };
    write_reply(&mut conn, &GuestReply::Ok)?;
    let _ = tcp.set_nodelay(true);
    let mut tcp_rd = tcp.try_clone()?;
    let mut tcp_wr = tcp;
    let mut conn_rd = conn.try_clone()?;
    let conn_wr = conn;
    let up = std::thread::spawn(move || {
        let mut conn_wr = conn_wr;
        let _ = io::copy(&mut tcp_rd, &mut conn_wr);
        conn_wr.shutdown_write();
    });
    let _ = io::copy(&mut conn_rd, &mut tcp_wr);
    let _ = tcp_wr.shutdown(std::net::Shutdown::Write);
    let _ = up.join();
    Ok(())
}

#[cfg(test)]
mod tests {
    //! The server over TCP in-process: the same code paths the vsock server
    //! runs, minus the socket family. Runs wherever Linux does (CI, the
    //! worker box, a container).
    use super::*;
    use crate::frames::read_frame;

    fn server() -> std::net::SocketAddr {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for s in listener.incoming().flatten() {
                let conn = Conn::from_fd(s.into_raw_fd());
                std::thread::spawn(move || handle(conn));
            }
        });
        addr
    }

    fn request(addr: std::net::SocketAddr, req: &GuestRequest, stdin: Option<&[u8]>) -> (Vec<u8>, Vec<u8>, GuestReply) {
        let mut s = std::net::TcpStream::connect(addr).unwrap();
        write_frame(&mut s, kind::JSON, &serde_json::to_vec(req).unwrap()).unwrap();
        if let Some(data) = stdin {
            write_frame(&mut s, kind::STDIN, data).unwrap();
            write_frame(&mut s, kind::STDIN_EOF, &[]).unwrap();
        }
        let (mut out, mut err) = (Vec::new(), Vec::new());
        loop {
            let (k, payload) = read_frame(&mut s).unwrap().expect("a reply");
            match k {
                kind::STDOUT => out.extend(payload),
                kind::STDERR => err.extend(payload),
                kind::JSON => return (out, err, serde_json::from_slice(&payload).unwrap()),
                _ => {}
            }
        }
    }

    fn exec_req(argv: &[&str], timeout_ms: Option<u64>, stdin: bool) -> GuestRequest {
        GuestRequest::Exec {
            argv: argv.iter().map(|s| s.to_string()).collect(),
            env: [("GREETING".to_string(), "hi".to_string())].into(),
            cwd: None,
            user: None,
            timeout_ms,
            stdin,
        }
    }

    #[test]
    fn exec_streams_output_and_reports_the_exit_code() {
        let addr = server();
        let (out, _, reply) = request(addr, &exec_req(&["sh", "-c", "echo $GREETING; exit 3"], None, false), None);
        assert_eq!(out, b"hi\n");
        assert_eq!(reply, GuestReply::Exited { code: 3, timed_out: false });
    }

    #[test]
    fn exec_feeds_stdin() {
        let addr = server();
        let (out, _, reply) = request(addr, &exec_req(&["cat"], None, true), Some(b"piped"));
        assert_eq!(out, b"piped");
        assert_eq!(reply, GuestReply::Exited { code: 0, timed_out: false });
    }

    #[test]
    fn a_timeout_kills_the_whole_group_and_reports_124() {
        let addr = server();
        let started = Instant::now();
        let (_, err, reply) =
            request(addr, &exec_req(&["sh", "-c", "sleep 30 & sleep 30"], Some(300), false), None);
        assert_eq!(reply, GuestReply::Exited { code: 124, timed_out: true });
        assert!(String::from_utf8_lossy(&err).contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(5), "the backgrounded child must not hold it open");
    }

    #[test]
    fn a_missing_program_is_127() {
        let addr = server();
        let (_, _, reply) = request(addr, &exec_req(&["/definitely/not/here"], None, false), None);
        assert_eq!(reply, GuestReply::Exited { code: 127, timed_out: false });
    }

    #[test]
    fn connect_splices_to_a_guest_port() {
        let echo = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = echo.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut s, _) = echo.accept().unwrap();
            let mut buf = [0u8; 5];
            s.read_exact(&mut buf).unwrap();
            s.write_all(&buf).unwrap();
        });
        let addr = server();
        let mut s = std::net::TcpStream::connect(addr).unwrap();
        write_frame(&mut s, kind::JSON, &serde_json::to_vec(&GuestRequest::Connect { port }).unwrap()).unwrap();
        let (_, ok) = read_frame(&mut s).unwrap().unwrap();
        assert_eq!(serde_json::from_slice::<GuestReply>(&ok).unwrap(), GuestReply::Ok);
        s.write_all(b"hello").unwrap();
        let mut back = [0u8; 5];
        s.read_exact(&mut back).unwrap();
        assert_eq!(&back, b"hello", "raw bytes after the Ok");
    }

    #[test]
    fn connecting_to_a_closed_port_is_an_error_reply() {
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let addr = server();
        let mut s = std::net::TcpStream::connect(addr).unwrap();
        write_frame(&mut s, kind::JSON, &serde_json::to_vec(&GuestRequest::Connect { port }).unwrap()).unwrap();
        let (_, reply) = read_frame(&mut s).unwrap().unwrap();
        assert!(matches!(serde_json::from_slice(&reply).unwrap(), GuestReply::Error { .. }));
    }

    #[test]
    fn users_resolve_from_numbers_and_pairs() {
        assert_eq!(resolve_user("1000:1001").map(|(u, g, _)| (u, g)), Some((1000, 1001)));
        assert_eq!(resolve_user("0").map(|(u, _, _)| u), Some(0));
        assert!(resolve_user("no-such-user-here").is_none());
    }
}
