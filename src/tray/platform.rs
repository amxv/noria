use super::Event;
use anyhow::Result;
#[cfg(target_os = "linux")]
use anyhow::{Context, bail};
use std::sync::mpsc::{self, Receiver, Sender};

#[derive(Clone)]
pub struct EventSender {
    tx: Sender<Event>,
    #[cfg(windows)]
    thread: u32,
}

impl EventSender {
    pub fn send(&self, event: Event) {
        if self.tx.send(event).is_ok() {
            #[cfg(windows)]
            unsafe {
                windows_sys::Win32::UI::WindowsAndMessaging::PostThreadMessageW(
                    self.thread,
                    WAKE,
                    0,
                    0,
                );
            }
        }
    }
}

#[cfg(windows)]
const WAKE: u32 = windows_sys::Win32::UI::WindowsAndMessaging::WM_APP + 19;

pub fn channel() -> (EventSender, Receiver<Event>) {
    let (tx, rx) = mpsc::channel();
    #[cfg(windows)]
    {
        use windows_sys::Win32::{
            System::Threading::GetCurrentThreadId,
            UI::WindowsAndMessaging::{MSG, PM_NOREMOVE, PeekMessageW},
        };
        // Windows creates a message queue lazily. Create it before any worker
        // can post an event so the first status/menu event cannot be lost.
        let thread = unsafe {
            let mut message: MSG = std::mem::zeroed();
            PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_NOREMOVE);
            GetCurrentThreadId()
        };
        (EventSender { tx, thread }, rx)
    }
    #[cfg(not(windows))]
    (EventSender { tx }, rx)
}

#[cfg(target_os = "linux")]
pub fn run(receiver: Receiver<Event>, mut event: impl FnMut(Event) -> bool) -> Result<()> {
    // ksni services native D-Bus events on its own worker. The UI thread sleeps
    // until an actual menu/file/command event or the existing 30-second refresh.
    loop {
        let next = match receiver.recv_timeout(std::time::Duration::from_secs(30)) {
            Ok(value) => value,
            Err(mpsc::RecvTimeoutError::Timeout) => Event::Tick,
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        };
        if !event(next) {
            return Ok(());
        }
    }
}

#[cfg(windows)]
pub fn run(receiver: Receiver<Event>, mut event: impl FnMut(Event) -> bool) -> Result<()> {
    use windows_sys::Win32::UI::WindowsAndMessaging::*;
    unsafe {
        let timer = SetTimer(std::ptr::null_mut(), 0, 30_000, None);
        if timer == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let result = (|| {
            let mut message: MSG = std::mem::zeroed();
            loop {
                let status = GetMessageW(&mut message, std::ptr::null_mut(), 0, 0);
                if status == -1 {
                    return Err(std::io::Error::last_os_error().into());
                }
                if status == 0 {
                    return Ok(());
                }
                if message.message == WM_TIMER && message.wParam == timer && !event(Event::Tick) {
                    return Ok(());
                }
                while let Ok(next) = receiver.try_recv() {
                    if !event(next) {
                        return Ok(());
                    }
                }
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        })();
        KillTimer(std::ptr::null_mut(), timer);
        result
    }
}

#[cfg(target_os = "linux")]
pub fn prepare() -> Result<()> {
    use std::{os::unix::fs::FileTypeExt, path::PathBuf};
    if std::env::var_os("DISPLAY").is_none_or(|value| value.is_empty())
        && std::env::var_os("WAYLAND_DISPLAY").is_none_or(|value| value.is_empty())
    {
        bail!("a graphical Linux desktop session is required");
    }
    if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none_or(|value| value.is_empty()) {
        let directory = PathBuf::from(
            std::env::var_os("XDG_RUNTIME_DIR").context("missing user D-Bus session address")?,
        );
        if !directory.is_absolute() {
            bail!("missing user D-Bus session address");
        }
        let socket = directory.join("bus");
        if !socket
            .metadata()
            .is_ok_and(|metadata| metadata.file_type().is_socket())
        {
            bail!("user D-Bus session socket is unavailable");
        }
        // Called once, before the tray, watcher, or async runtime starts threads.
        unsafe {
            std::env::set_var(
                "DBUS_SESSION_BUS_ADDRESS",
                format!(
                    "unix:path={}",
                    socket.display().to_string().replace(',', "%2C")
                ),
            );
        }
    }
    Ok(())
}

#[cfg(windows)]
pub fn prepare() -> Result<()> {
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn check_host() -> Result<()> {
    prepare()?;
    let bus = zbus::blocking::Connection::session().context("connect to the desktop D-Bus")?;
    let proxy = zbus::blocking::fdo::DBusProxy::new(&bus)?;
    if proxy.name_has_owner("org.kde.StatusNotifierWatcher".try_into()?)? {
        return Ok(());
    }
    if std::env::var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .to_uppercase()
        .contains("GNOME")
    {
        bail!(
            "GNOME's AppIndicator extension is not active. Ubuntu Desktop includes it; on other GNOME desktops, enable 'AppIndicator and KStatusNotifierItem Support' in Extensions"
        );
    }
    bail!("this desktop has no active StatusNotifierItem tray host; enable its tray/panel applet")
}

#[cfg(windows)]
pub fn check_host() -> Result<()> {
    Ok(())
}
