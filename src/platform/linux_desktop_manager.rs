use super::{linux::*, ResultType};
use crate::client::{
    LOGIN_MSG_DESKTOP_NO_DESKTOP, LOGIN_MSG_DESKTOP_SESSION_ANOTHER_USER,
    LOGIN_MSG_DESKTOP_SESSION_NOT_READY, LOGIN_MSG_DESKTOP_XORG_NOT_FOUND,
    LOGIN_MSG_DESKTOP_XSESSION_FAILED,
};
use hbb_common::{
    allow_err, bail, log,
    rand::prelude::*,
    tokio::time,
    users::{get_user_by_name, os::unix::UserExt, User},
};
use pam;
use std::{
    collections::HashMap,
    os::unix::process::CommandExt,
    path::Path,
    process::{Child, Command},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{sync_channel, SyncSender},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

lazy_static::lazy_static! {
    static ref DESKTOP_RUNNING: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    static ref DESKTOP_MANAGER: Arc<Mutex<Option<DesktopManager>>> = Arc::new(Mutex::new(None));
}

#[derive(Debug)]
struct DesktopManager {
    seat0_username: String,
    seat0_display_server: String,
    child_username: String,
    child_exit: Arc<AtomicBool>,
    is_child_running: Arc<AtomicBool>,
}

fn check_desktop_manager() {
    let mut desktop_manager = DESKTOP_MANAGER.lock().unwrap();
    if let Some(desktop_manager) = &mut (*desktop_manager) {
        if desktop_manager.is_child_running.load(Ordering::SeqCst) {
            return;
        }
        desktop_manager.child_exit.store(true, Ordering::SeqCst);
    }
}

pub fn start_xdesktop() {
    debug_assert!(crate::is_server());
    std::thread::spawn(|| {
        *DESKTOP_MANAGER.lock().unwrap() = Some(DesktopManager::new());

        let interval = time::Duration::from_millis(super::SERVICE_INTERVAL);
        DESKTOP_RUNNING.store(true, Ordering::SeqCst);
        while DESKTOP_RUNNING.load(Ordering::SeqCst) {
            check_desktop_manager();
            std::thread::sleep(interval);
        }
        log::info!("xdesktop child thread exit");
    });
}

pub fn stop_xdesktop() {
    DESKTOP_RUNNING.store(false, Ordering::SeqCst);
    *DESKTOP_MANAGER.lock().unwrap() = None;
}

fn detect_headless() -> Option<&'static str> {
    match run_cmds(&format!("which {}", DesktopManager::get_xorg())) {
        Ok(output) => {
            if output.trim().is_empty() {
                return Some(LOGIN_MSG_DESKTOP_XORG_NOT_FOUND);
            }
        }
        _ => {
            return Some(LOGIN_MSG_DESKTOP_XORG_NOT_FOUND);
        }
    }

    match run_cmds("ls /usr/share/xsessions/") {
        Ok(output) => {
            if output.trim().is_empty() {
                return Some(LOGIN_MSG_DESKTOP_NO_DESKTOP);
            }
        }
        _ => {
            return Some(LOGIN_MSG_DESKTOP_NO_DESKTOP);
        }
    }

    None
}

pub fn try_start_desktop(_username: &str, _passsword: &str) -> String {
    debug_assert!(crate::is_server());

    // If we're at a Wayland login screen and have credentials, start a Wayland session
    if is_login_screen_wayland() {
        if _username.is_empty() {
            return LOGIN_MSG_DESKTOP_SESSION_NOT_READY.to_owned();
        }
        log::info!("try_start_desktop: Wayland login screen, attempting session for '{}'", _username);
        return match try_start_wayland_session(_username, _passsword) {
            Ok(session_ready) => {
                if session_ready {
                    "".to_owned()
                } else {
                    LOGIN_MSG_DESKTOP_SESSION_NOT_READY.to_owned()
                }
            }
            Err(e) => {
                log::error!("Failed to start Wayland session: {}", e);
                LOGIN_MSG_DESKTOP_XSESSION_FAILED.to_owned()
            }
        };
    }

    if _username.is_empty() {
        let username = get_username();
        if username.is_empty() {
            if let Some(msg) = detect_headless() {
                msg
            } else {
                LOGIN_MSG_DESKTOP_SESSION_NOT_READY
            }
        } else {
            ""
        }
        .to_owned()
    } else {
        let username = get_username();
        if username == _username {
            // No need to verify password here.
            return "".to_owned();
        }
        if !username.is_empty() {
            // Another user is logged in. No need to start a new xsession.
            return "".to_owned();
        }

        if let Some(msg) = detect_headless() {
            return msg.to_owned();
        }

        match try_start_x_session(_username, _passsword) {
            Ok((username, x11_ready)) => {
                if x11_ready {
                    if _username != username {
                        LOGIN_MSG_DESKTOP_SESSION_ANOTHER_USER.to_owned()
                    } else {
                        "".to_owned()
                    }
                } else {
                    LOGIN_MSG_DESKTOP_SESSION_NOT_READY.to_owned()
                }
            }
            Err(e) => {
                log::error!("Failed to start xsession {}", e);
                LOGIN_MSG_DESKTOP_XSESSION_FAILED.to_owned()
            }
        }
    }
}

fn try_start_x_session(username: &str, password: &str) -> ResultType<(String, bool)> {
    let mut desktop_manager = DESKTOP_MANAGER.lock().unwrap();
    if let Some(desktop_manager) = &mut (*desktop_manager) {
        if let Some(seat0_username) = desktop_manager.get_supported_display_seat0_username() {
            return Ok((seat0_username, true));
        }

        let _ = desktop_manager.try_start_x_session(username, password)?;
        log::debug!(
            "try_start_x_session, username: {}, {:?}",
            &username,
            &desktop_manager
        );
        Ok((
            desktop_manager.child_username.clone(),
            desktop_manager.is_running(),
        ))
    } else {
        bail!(crate::client::LOGIN_MSG_DESKTOP_NOT_INITED);
    }
}

#[inline]
pub fn is_headless() -> bool {
    DESKTOP_MANAGER
        .lock()
        .unwrap()
        .as_ref()
        .map_or(false, |manager| {
            manager.get_supported_display_seat0_username().is_none()
        })
}

pub fn get_username() -> String {
    match &*DESKTOP_MANAGER.lock().unwrap() {
        Some(manager) => {
            if let Some(seat0_username) = manager.get_supported_display_seat0_username() {
                seat0_username
            } else {
                if manager.is_running() && !manager.child_username.is_empty() {
                    manager.child_username.clone()
                } else {
                    "".to_owned()
                }
            }
        }
        None => "".to_owned(),
    }
}

impl Drop for DesktopManager {
    fn drop(&mut self) {
        self.stop_children();
    }
}

impl DesktopManager {
    fn fatal_exit() {
        std::process::exit(0);
    }

    pub fn new() -> Self {
        let mut seat0_username = "".to_owned();
        let mut seat0_display_server = "".to_owned();
        let seat0_values = get_values_of_seat0(&[0, 2]);
        if !seat0_values[0].is_empty() {
            seat0_username = seat0_values[1].clone();
            seat0_display_server = get_display_server_of_session(&seat0_values[0]);
        }
        Self {
            seat0_username,
            seat0_display_server,
            child_username: "".to_owned(),
            child_exit: Arc::new(AtomicBool::new(true)),
            is_child_running: Arc::new(AtomicBool::new(false)),
        }
    }

    fn get_supported_display_seat0_username(&self) -> Option<String> {
        if is_gdm_user(&self.seat0_username) && self.seat0_display_server == DISPLAY_SERVER_WAYLAND
        {
            None
        } else if self.seat0_username.is_empty() {
            None
        } else {
            Some(self.seat0_username.clone())
        }
    }

    #[inline]
    fn get_xauth() -> String {
        let xauth = get_env_var("XAUTHORITY");
        if xauth.is_empty() {
            "/tmp/.Xauthority".to_owned()
        } else {
            xauth
        }
    }

    #[inline]
    fn is_running(&self) -> bool {
        self.is_child_running.load(Ordering::SeqCst)
    }

    fn try_start_x_session(&mut self, username: &str, password: &str) -> ResultType<()> {
        match get_user_by_name(username) {
            Some(userinfo) => {
                let mut client = pam::Client::with_password(&pam_get_service_name())?;
                client
                    .conversation_mut()
                    .set_credentials(username, password);
                match client.authenticate() {
                    Ok(_) => {
                        if self.is_running() {
                            return Ok(());
                        }

                        match self.start_x_session(&userinfo, username, password) {
                            Ok(_) => {
                                log::info!("Succeeded to start x11");
                                self.child_username = username.to_string();
                                Ok(())
                            }
                            Err(e) => {
                                bail!("failed to start x session, {}", e);
                            }
                        }
                    }
                    Err(e) => {
                        bail!("failed to check user pass for {}, {}", username, e);
                    }
                }
            }
            None => {
                bail!("failed to get userinfo of {}", username);
            }
        }
    }

    // The logic mainly from https://github.com/neutrinolabs/xrdp/blob/34fe9b60ebaea59e8814bbc3ca5383cabaa1b869/sesman/session.c#L334.
    fn get_avail_display() -> ResultType<u32> {
        let display_range = 0..51;
        for i in display_range.clone() {
            if Self::is_x_server_running(i) {
                continue;
            }
            return Ok(i);
        }
        bail!("No available display found in range {:?}", display_range)
    }

    #[inline]
    fn is_x_server_running(display: u32) -> bool {
        Path::new(&format!("/tmp/.X11-unix/X{}", display)).exists()
            || Path::new(&format!("/tmp/.X{}-lock", display)).exists()
    }

    fn start_x_session(
        &mut self,
        userinfo: &User,
        username: &str,
        password: &str,
    ) -> ResultType<()> {
        self.stop_children();

        let display_num = Self::get_avail_display()?;
        // "xServer_ip:display_num.screen_num"

        let uid = userinfo.uid();
        let gid = userinfo.primary_group_id();
        let envs = HashMap::from([
            ("SHELL", userinfo.shell().to_string_lossy().to_string()),
            ("PATH", "/sbin:/bin:/usr/bin:/usr/local/bin".to_owned()),
            ("USER", username.to_string()),
            ("UID", userinfo.uid().to_string()),
            ("HOME", userinfo.home_dir().to_string_lossy().to_string()),
            (
                "XDG_RUNTIME_DIR",
                format!("/run/user/{}", userinfo.uid().to_string()),
            ),
            // ("DISPLAY", self.display.clone()),
            // ("XAUTHORITY", self.xauth.clone()),
            // (ENV_DESKTOP_PROTOCOL, XProtocol::X11.to_string()),
        ]);
        self.child_exit.store(false, Ordering::SeqCst);
        let is_child_running = self.is_child_running.clone();

        let (tx_res, rx_res) = sync_channel(1);
        let password = password.to_string();
        let username = username.to_string();
        // start x11
        std::thread::spawn(move || {
            match Self::start_x_session_thread(
                tx_res.clone(),
                is_child_running,
                uid,
                gid,
                display_num,
                username,
                password,
                envs,
            ) {
                Ok(_) => {}
                Err(e) => {
                    log::error!("Failed to start x session thread");
                    allow_err!(tx_res.send(format!("Failed to start x session thread, {}", e)));
                }
            }
        });

        // wait x11
        match rx_res.recv_timeout(Duration::from_millis(10_000)) {
            Ok(res) => {
                if res == "" {
                    Ok(())
                } else {
                    bail!(res)
                }
            }
            Err(e) => {
                bail!("Failed to recv x11 result {}", e)
            }
        }
    }

    #[inline]
    fn display_from_num(num: u32) -> String {
        format!(":{num}")
    }

    fn start_x_session_thread(
        tx_res: SyncSender<String>,
        is_child_running: Arc<AtomicBool>,
        uid: u32,
        gid: u32,
        display_num: u32,
        username: String,
        password: String,
        envs: HashMap<&str, String>,
    ) -> ResultType<()> {
        let mut client = pam::Client::with_password(&pam_get_service_name())?;
        client
            .conversation_mut()
            .set_credentials(&username, &password);
        client.authenticate()?;

        client.set_item(pam::PamItemType::TTY, &Self::display_from_num(display_num))?;
        client.open_session()?;

        // fixme: FreeBSD kernel needs to login here.
        // see: https://github.com/neutrinolabs/xrdp/blob/a64573b596b5fb07ca3a51590c5308d621f7214e/sesman/session.c#L556

        let (child_xorg, child_wm) = Self::start_x11(uid, gid, username, display_num, &envs)?;
        is_child_running.store(true, Ordering::SeqCst);

        log::info!("Start xorg and wm done, notify and wait xtop x11");
        allow_err!(tx_res.send("".to_owned()));

        Self::wait_stop_x11(child_xorg, child_wm);
        log::info!("Wait x11 stop done");
        Ok(())
    }

    fn wait_xorg_exit(child_xorg: &mut Child) -> ResultType<String> {
        if let Ok(_) = child_xorg.kill() {
            for _ in 0..3 {
                match child_xorg.try_wait() {
                    Ok(Some(status)) => return Ok(format!("Xorg exit with {}", status)),
                    Ok(None) => {}
                    Err(e) => {
                        // fatal error
                        log::error!("Failed to wait xorg process, {}", e);
                        bail!("Failed to wait xorg process, {}", e)
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(1_000));
            }
            log::error!("Failed to wait xorg process, not exit");
            bail!("Failed to wait xorg process, not exit")
        } else {
            Ok("Xorg is already exited".to_owned())
        }
    }

    fn add_xauth_cookie(
        file: &str,
        display: &str,
        uid: u32,
        gid: u32,
        envs: &HashMap<&str, String>,
    ) -> ResultType<()> {
        let randstr = (0..16)
            .map(|_| format!("{:02x}", random::<u8>()))
            .collect::<String>();
        let output = Command::new("xauth")
            .uid(uid)
            .gid(gid)
            .envs(envs)
            .args(vec!["-q", "-f", file, "add", display, ".", &randstr])
            .output()?;
        // xauth run success, even the following error occurs.
        // Ok(Output { status: ExitStatus(unix_wait_status(0)), stdout: "", stderr: "xauth:  file .Xauthority does not exist\n" })
        let errmsg = String::from_utf8_lossy(&output.stderr).to_string();
        if !errmsg.is_empty() {
            if !errmsg.contains("does not exist") {
                bail!("Failed to launch xauth, {}", errmsg)
            }
        }
        Ok(())
    }

    fn wait_x_server_running(pid: u32, display_num: u32, max_wait_secs: u64) -> ResultType<()> {
        let wait_begin = Instant::now();
        loop {
            if run_cmds(&format!("ls /proc/{}", pid))?.is_empty() {
                bail!("X server exit");
            }

            if Self::is_x_server_running(display_num) {
                return Ok(());
            }
            if wait_begin.elapsed().as_secs() > max_wait_secs {
                bail!("Failed to wait xserver after {} seconds", max_wait_secs);
            }
            std::thread::sleep(Duration::from_millis(300));
        }
    }

    fn start_x11(
        uid: u32,
        gid: u32,
        username: String,
        display_num: u32,
        envs: &HashMap<&str, String>,
    ) -> ResultType<(Child, Child)> {
        log::debug!("envs of user {}: {:?}", &username, &envs);

        let xauth = Self::get_xauth();
        let display = Self::display_from_num(display_num);

        Self::add_xauth_cookie(&xauth, &display, uid, gid, &envs)?;

        // Start Xorg
        let mut child_xorg = Self::start_x_server(&xauth, &display, uid, gid, &envs)?;

        log::info!("xorg started, wait 10 secs to ensuer x server is running");

        let max_wait_secs = 10;
        // wait x server running
        if let Err(e) = Self::wait_x_server_running(child_xorg.id(), display_num, max_wait_secs) {
            match Self::wait_xorg_exit(&mut child_xorg) {
                Ok(msg) => log::info!("{}", msg),
                Err(e) => {
                    log::error!("{}", e);
                    Self::fatal_exit();
                }
            }
            bail!(e)
        }

        log::info!(
            "xorg is running, start x window manager with DISPLAY: {}, XAUTHORITY: {}",
            &display,
            &xauth
        );

        std::env::set_var("DISPLAY", &display);
        std::env::set_var("XAUTHORITY", &xauth);
        // start window manager (startwm.sh)
        let child_wm = match Self::start_x_window_manager(uid, gid, &envs) {
            Ok(c) => c,
            Err(e) => {
                match Self::wait_xorg_exit(&mut child_xorg) {
                    Ok(msg) => log::info!("{}", msg),
                    Err(e) => {
                        log::error!("{}", e);
                        Self::fatal_exit();
                    }
                }
                bail!(e)
            }
        };
        log::info!("x window manager is started");

        Ok((child_xorg, child_wm))
    }

    fn try_wait_x11_child_exit(child_xorg: &mut Child, child_wm: &mut Child) -> bool {
        match child_xorg.try_wait() {
            Ok(Some(status)) => {
                log::info!("Xorg exit with {}", status);
                return true;
            }
            Ok(None) => {}
            Err(e) => log::error!("Failed to wait xorg process, {}", e),
        }

        match child_wm.try_wait() {
            Ok(Some(status)) => {
                // Logout may result "wm exit with signal: 11 (SIGSEGV) (core dumped)"
                log::info!("wm exit with {}", status);
                return true;
            }
            Ok(None) => {}
            Err(e) => log::error!("Failed to wait xorg process, {}", e),
        }
        false
    }

    fn wait_x11_children_exit(child_xorg: &mut Child, child_wm: &mut Child) {
        log::debug!("Try kill child process xorg");
        if let Ok(_) = child_xorg.kill() {
            let mut exited = false;
            for _ in 0..2 {
                match child_xorg.try_wait() {
                    Ok(Some(status)) => {
                        log::info!("Xorg exit with {}", status);
                        exited = true;
                        break;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        log::error!("Failed to wait xorg process, {}", e);
                        Self::fatal_exit();
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(1_000));
            }
            if !exited {
                log::error!("Failed to wait child xorg, after kill()");
                // try kill -9?
            }
        }
        log::debug!("Try kill child process wm");
        if let Ok(_) = child_wm.kill() {
            let mut exited = false;
            for _ in 0..2 {
                match child_wm.try_wait() {
                    Ok(Some(status)) => {
                        // Logout may result "wm exit with signal: 11 (SIGSEGV) (core dumped)"
                        log::info!("wm exit with {}", status);
                        exited = true;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        log::error!("Failed to wait wm process, {}", e);
                        Self::fatal_exit();
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(1_000));
            }
            if !exited {
                log::error!("Failed to wait child xorg, after kill()");
                // try kill -9?
            }
        }
    }

    fn try_wait_stop_x11(child_xorg: &mut Child, child_wm: &mut Child) -> bool {
        let mut desktop_manager = DESKTOP_MANAGER.lock().unwrap();
        let mut exited = true;
        if let Some(desktop_manager) = &mut (*desktop_manager) {
            if desktop_manager.child_exit.load(Ordering::SeqCst) {
                exited = true;
            } else {
                exited = Self::try_wait_x11_child_exit(child_xorg, child_wm);
            }
            if exited {
                log::debug!("Wait x11 children exiting");
                Self::wait_x11_children_exit(child_xorg, child_wm);
                desktop_manager
                    .is_child_running
                    .store(false, Ordering::SeqCst);
                desktop_manager.child_exit.store(true, Ordering::SeqCst);
            }
        }
        exited
    }

    fn wait_stop_x11(mut child_xorg: Child, mut child_wm: Child) {
        loop {
            if Self::try_wait_stop_x11(&mut child_xorg, &mut child_wm) {
                break;
            }
            std::thread::sleep(Duration::from_millis(super::SERVICE_INTERVAL));
        }
    }

    fn get_xorg() -> &'static str {
        // Fedora 26 or later
        let xorg = "/usr/libexec/Xorg";
        if Path::new(xorg).is_file() {
            return xorg;
        }
        // Debian 9 or later
        let xorg = "/usr/lib/xorg/Xorg";
        if Path::new(xorg).is_file() {
            return xorg;
        }
        // Ubuntu 16.04 or later
        let xorg = "/usr/lib/xorg/Xorg";
        if Path::new(xorg).is_file() {
            return xorg;
        }
        // Arch Linux
        let xorg = "/usr/lib/xorg-server/Xorg";
        if Path::new(xorg).is_file() {
            return xorg;
        }
        // Arch Linux
        let xorg = "/usr/lib/Xorg";
        if Path::new(xorg).is_file() {
            return xorg;
        }
        // CentOS 7 /usr/bin/Xorg or param=Xorg

        log::warn!("Failed to find xorg, use default Xorg.\n Please add \"allowed_users=anybody\" to \"/etc/X11/Xwrapper.config\".");
        "Xorg"
    }

    fn start_x_server(
        xauth: &str,
        display: &str,
        uid: u32,
        gid: u32,
        envs: &HashMap<&str, String>,
    ) -> ResultType<Child> {
        let xorg = Self::get_xorg();
        log::info!("Use xorg: {}", &xorg);
        let app_name = crate::get_app_name().to_lowercase();
        let conf = format!("/etc/{app_name}/xorg.conf");
        match Command::new(xorg)
            .envs(envs)
            .uid(uid)
            .gid(gid)
            .args(vec![
                "-noreset",
                "+extension",
                "GLX",
                "+extension",
                "RANDR",
                "+extension",
                "RENDER",
                "-config",
                conf.as_ref(),
                "-auth",
                xauth,
                display,
            ])
            .spawn()
        {
            Ok(c) => Ok(c),
            Err(e) => {
                bail!("Failed to start Xorg with display {}, {}", display, e);
            }
        }
    }

    fn start_x_window_manager(
        uid: u32,
        gid: u32,
        envs: &HashMap<&str, String>,
    ) -> ResultType<Child> {
        let app_name = crate::get_app_name().to_lowercase();
        match Command::new(&format!("/etc/{app_name}/startwm.sh"))
            .envs(envs)
            .uid(uid)
            .gid(gid)
            .spawn()
        {
            Ok(c) => Ok(c),
            Err(e) => {
                bail!("Failed to start window manager, {}", e);
            }
        }
    }

    fn stop_children(&mut self) {
        self.child_exit.store(true, Ordering::SeqCst);
        for _i in 1..10 {
            if !self.is_child_running.load(Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(Duration::from_millis(super::SERVICE_INTERVAL));
        }
        if self.is_child_running.load(Ordering::SeqCst) {
            log::warn!("xdesktop child is still running!");
        }
    }
}

/// Attempt to start a Wayland desktop session for the given user via PAM + loginctl/systemd.
///
/// Returns Ok(true) if a desktop session is already active for the user,
/// Ok(false) if session start was initiated but not yet ready (client should retry).
pub fn debug_log(msg: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open("/tmp/rustdesk_debug.log") {
        let _ = writeln!(f, "{:?} {}", std::time::SystemTime::now(), msg);
    }
}

fn try_start_wayland_session(username: &str, password: &str) -> ResultType<bool> {
    debug_log(&format!("try_start_wayland_session: user='{}', password_len={}", username, password.len()));

    let userinfo = match get_user_by_name(username) {
        Some(u) => {
            debug_log(&format!("try_start_wayland_session: found user uid={}", u.uid()));
            u
        }
        None => {
            debug_log(&format!("try_start_wayland_session: user '{}' NOT FOUND", username));
            bail!("User '{}' not found", username);
        }
    };

    // PAM authenticate — use 'login' service which is always available
    let pam_service = if Path::new("/etc/pam.d/login").is_file() {
        "login".to_owned()
    } else {
        pam_get_service_name()
    };
    debug_log(&format!("try_start_wayland_session: PAM service='{}', authenticating...", pam_service));
    let mut client = pam::Client::with_password(&pam_service)?;
    client
        .conversation_mut()
        .set_credentials(username, password);
    match client.authenticate() {
        Ok(_) => debug_log("try_start_wayland_session: PAM authenticate OK"),
        Err(ref e) => {
            debug_log(&format!("try_start_wayland_session: PAM authenticate FAILED: {}", e));
            bail!("PAM authentication failed for '{}': {}", username, e);
        }
    }
    // Skip pam_open_session() — it creates a lingering logind session we don't need.
    // The real session is created by the display manager when it restarts with autologin.

    // PAM succeeded. Write a signal file so the service daemon (start_os_service loop)
    // can handle session activation — but only if one isn't already pending.
    let signal_path = wayland_session_signal_path();
    if Path::new(&signal_path).exists() {
        debug_log("try_start_wayland_session: signal file already exists — activation in progress, skipping");
        return Ok(false);
    }

    let session_exec = get_user_session_exec(username);
    let signal_content = format!("{}\n{}\n{}", username, userinfo.uid(), session_exec);
    debug_log(&format!("try_start_wayland_session: writing signal file to {} (session_exec='{}')", signal_path, session_exec));

    if let Err(e) = std::fs::write(&signal_path, &signal_content) {
        debug_log(&format!("try_start_wayland_session: failed to write signal file: {}", e));
        bail!("Failed to write session signal file: {}", e);
    }

    debug_log("try_start_wayland_session: signal file written, returning Ok(false) — service loop will activate session");
    Ok(false)
}

/// Determine the session executable for a user's preferred Wayland session.
///
/// Checks (in order):
/// 1. AccountsService user config (`/var/lib/AccountsService/users/$USER`)
/// 2. Available `.desktop` files in `/usr/share/wayland-sessions/`
/// 3. Fallback to `gnome-session` (most common)
fn get_user_session_exec(username: &str) -> String {
    // Try AccountsService
    let accounts_file = format!("/var/lib/AccountsService/users/{}", username);
    if let Ok(contents) = std::fs::read_to_string(&accounts_file) {
        // Look for Session= or XSession= key
        for line in contents.lines() {
            let line = line.trim();
            if line.starts_with("Session=") || line.starts_with("XSession=") {
                if let Some(session_name) = line.split('=').nth(1) {
                    let session_name = session_name.trim();
                    if !session_name.is_empty() {
                        if let Some(exec) = resolve_wayland_session_exec(session_name) {
                            return exec;
                        }
                    }
                }
            }
        }
    }

    // Scan /usr/share/wayland-sessions/ for any available session
    if let Ok(entries) = std::fs::read_dir("/usr/share/wayland-sessions") {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().map_or(false, |e| e == "desktop") {
                if let Some(exec) = parse_desktop_exec(&path) {
                    return exec;
                }
            }
        }
    }

    // Fallback
    log::warn!("No Wayland session found, falling back to gnome-session");
    "gnome-session".to_owned()
}

/// Look up a session name in wayland-sessions .desktop files and return its Exec= line.
fn resolve_wayland_session_exec(session_name: &str) -> Option<String> {
    let desktop_file = format!(
        "/usr/share/wayland-sessions/{}.desktop",
        session_name
    );
    if Path::new(&desktop_file).is_file() {
        return parse_desktop_exec(Path::new(&desktop_file));
    }
    None
}

/// Parse the Exec= line from a .desktop file.
fn parse_desktop_exec(path: &Path) -> Option<String> {
    if let Ok(contents) = std::fs::read_to_string(path) {
        for line in contents.lines() {
            let line = line.trim();
            if line.starts_with("Exec=") {
                let exec = line.strip_prefix("Exec=").unwrap_or("").trim();
                if !exec.is_empty() {
                    return Some(exec.to_owned());
                }
            }
        }
    }
    None
}

/// Path for the session activation signal file.
/// Written by the connection handler, read by start_os_service() loop.
pub fn wayland_session_signal_path() -> String {
    "/tmp/rustdesk-wayland-session-request".to_owned()
}

/// Read and parse the session activation signal file.
/// Returns Some((username, uid, session_exec)) if a request is pending.
pub fn read_wayland_session_signal() -> Option<(String, u32, String)> {
    let path = wayland_session_signal_path();
    if !Path::new(&path).exists() {
        return None;
    }
    match std::fs::read_to_string(&path) {
        Ok(contents) => {
            let lines: Vec<&str> = contents.trim().lines().collect();
            if lines.len() >= 3 {
                let username = lines[0].to_string();
                let uid: u32 = lines[1].parse().unwrap_or(0);
                let session_exec = lines[2].to_string();
                if !username.is_empty() && uid > 0 && !session_exec.is_empty() {
                    return Some((username, uid, session_exec));
                }
            }
            // Malformed signal file — remove it
            let _ = std::fs::remove_file(&path);
            None
        }
        Err(_) => None,
    }
}

/// Remove the session activation signal file.
pub fn clear_wayland_session_signal() {
    let _ = std::fs::remove_file(&wayland_session_signal_path());
}

/// Detect which display manager systemd service is running.
/// Returns None if no known DM is detected — caller should not proceed with activation.
pub fn detect_display_manager_service() -> Option<String> {
    for dm in &["gdm", "gdm3", "sddm", "lightdm", "lxdm", "xdm"] {
        if let Ok(output) = run_cmds(&format!("systemctl is-active {}", dm)) {
            if output.trim() == "active" {
                return Some(dm.to_string());
            }
        }
    }
    // Fallback: check display-manager.service alias
    if let Ok(output) = run_cmds("systemctl is-active display-manager") {
        if output.trim() == "active" {
            return Some("display-manager".to_string());
        }
    }
    None
}

fn pam_get_service_name() -> String {
    let app_name = crate::get_app_name().to_lowercase();
    if Path::new(&format!("/etc/pam.d/{app_name}")).is_file() {
        app_name
    } else {
        "gdm".to_owned()
    }
}
