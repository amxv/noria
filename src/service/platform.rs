use super::*;

#[cfg(target_os = "linux")]
async fn systemd_available() -> bool {
    // A test/overridden HOME must never reach the actual user's service manager.
    let Ok(selected) = user_home().and_then(|p| Ok(p.canonicalize()?)) else {
        return false;
    };
    // SAFETY: copy the passwd field immediately, before any other libc lookup.
    let actual = unsafe {
        let pw = libc::getpwuid(libc::getuid());
        if pw.is_null() {
            return false;
        }
        PathBuf::from(
            std::ffi::CStr::from_ptr((*pw).pw_dir)
                .to_string_lossy()
                .as_ref(),
        )
    };
    if actual.canonicalize().ok().as_ref() != Some(&selected) {
        return false;
    }
    run("systemctl", &["--user".into(), "show-environment".into()])
        .await
        .is_ok()
}

#[cfg(any(target_os = "linux", test))]
pub fn systemd_quote(s: &str) -> String {
    format!(
        "\"{}\"",
        s.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
            .replace('$', "$$")
            .replace(['\n', '\r'], " ")
    )
}

#[cfg(target_os = "linux")]
async fn install_unit(home: &Path, exe: &Path, c: &Config) -> Result<()> {
    let user = user_home()?;
    let content = format!(
        "[Unit]\nDescription=Noria local account router\nAfter=network.target\n[Service]\nType=simple\nExecStart={} __daemon --home {}\nWorkingDirectory={}\nEnvironment={}\nEnvironment={}\nRestart=on-failure\nRestartSec=3\n[Install]\nWantedBy=default.target\n",
        systemd_quote(&exe.to_string_lossy()),
        systemd_quote(&home.to_string_lossy()),
        systemd_quote(&home.to_string_lossy()),
        systemd_quote(&format!("HOME={}", user.display())),
        systemd_quote(&format!("PATH={}", c.tool_path))
    );
    user_file(
        &user
            .join(".config/systemd/user")
            .join(format!("{}.service", label(home))),
        content.as_bytes(),
    )?;
    run("systemctl", &["--user".into(), "daemon-reload".into()]).await
}

#[cfg(target_os = "macos")]
fn domain() -> String {
    format!("gui/{}", unsafe { libc::getuid() })
}

#[cfg(target_os = "macos")]
fn launch_plist(home: &Path, exe: &Path, c: &Config) -> Result<String> {
    let h = xml(&home.to_string_lossy());
    let log = xml(&home.join("logs/service.log").to_string_lossy());
    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><plist version=\"1.0\"><dict><key>Label</key><string>{}</string><key>ProgramArguments</key><array><string>{}</string><string>__daemon</string><string>--home</string><string>{h}</string></array><key>RunAtLoad</key><true/><key>KeepAlive</key><true/><key>ThrottleInterval</key><integer>3</integer><key>ProcessType</key><string>Interactive</string><key>WorkingDirectory</key><string>{h}</string><key>EnvironmentVariables</key><dict><key>HOME</key><string>{}</string><key>NORIA_HOME</key><string>{h}</string><key>PATH</key><string>{}</string></dict><key>StandardOutPath</key><string>{log}</string><key>StandardErrorPath</key><string>{log}</string></dict></plist>\n",
        label(home),
        xml(&exe.to_string_lossy()),
        xml(&user_home()?.to_string_lossy()),
        xml(&c.tool_path)
    ))
}

pub async fn start(home: &Path, exe: &Path, c: &Config) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        if systemd_available().await {
            install_unit(home, exe, c).await?;
            if run(
                "systemctl",
                &[
                    "--user".into(),
                    "start".into(),
                    format!("{}.service", label(home)),
                ],
            )
            .await
            .is_ok()
            {
                return Ok(());
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        if run("/bin/launchctl", &["print".into(), domain()])
            .await
            .is_ok()
        {
            drop(log_file(home, "service.log")?);
            let path = home.join("service.plist");
            config::write(&path, launch_plist(home, exe, c)?.as_bytes())?;
            let job = format!("{}/{}", domain(), label(home));
            if run("/bin/launchctl", &["print".into(), job.clone()])
                .await
                .is_ok()
            {
                return run("/bin/launchctl", &["kickstart".into(), job]).await;
            }
            return run(
                "/bin/launchctl",
                &[
                    "bootstrap".into(),
                    domain(),
                    path.to_string_lossy().into_owned(),
                ],
            )
            .await;
        }
    }
    detach(home, exe, c).await
}

pub async fn stop(home: &Path) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        if systemd_available().await {
            let job = format!("{}.service", label(home));
            if run("systemctl", &["--user".into(), "show".into(), job.clone()])
                .await
                .is_ok()
            {
                return run("systemctl", &["--user".into(), "stop".into(), job]).await;
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        let job = format!("{}/{}", domain(), label(home));
        if run("/bin/launchctl", &["print".into(), job.clone()])
            .await
            .is_ok()
        {
            return run("/bin/launchctl", &["bootout".into(), job]).await;
        }
    }
    let _ = home;
    Ok(())
}

pub async fn login(home: &Path, exe: &Path, enabled: bool) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        if !systemd_available().await {
            bail!(
                "login startup needs a systemd user session on this Linux host; start/stop still work"
            );
        }
        install_unit(home, exe, &config::load(home)?).await?;
        run(
            "systemctl",
            &[
                "--user".into(),
                if enabled { "enable" } else { "disable" }.into(),
                format!("{}.service", label(home)),
            ],
        )
        .await
    }
    #[cfg(target_os = "macos")]
    {
        let path = user_home()?
            .join("Library/LaunchAgents")
            .join(format!("{}.plist", label(home)));
        if !enabled {
            if path.exists() {
                fs::remove_file(path)?;
            }
            return Ok(());
        }
        drop(log_file(home, "service.log")?);
        user_file(
            &path,
            launch_plist(home, exe, &config::load(home)?)?.as_bytes(),
        )
    }
    #[cfg(windows)]
    {
        let name = label(home);
        if !enabled {
            let _ = run(
                "schtasks.exe",
                &["/Delete".into(), "/TN".into(), name, "/F".into()],
            )
            .await;
            return Ok(());
        }
        let output = Command::new("whoami.exe")
            .args(["/user", "/fo", "csv", "/nh"])
            .output()
            .await?;
        if !output.status.success() {
            bail!("could not identify current Windows user");
        }
        let text = String::from_utf8(output.stdout)?;
        let sid = text
            .trim()
            .split(',')
            .nth(1)
            .map(|s| s.trim_matches('"'))
            .context("could not read current user SID")?;
        if !sid.starts_with("S-1-") {
            bail!("invalid current user SID");
        }
        let xml = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Task version=\"1.2\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\"><Triggers><LogonTrigger><Enabled>true</Enabled><UserId>{sid}</UserId></LogonTrigger></Triggers><Principals><Principal id=\"Author\"><UserId>{sid}</UserId><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal></Principals><Settings><MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy><DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries><StopIfGoingOnBatteries>false</StopIfGoingOnBatteries><ExecutionTimeLimit>PT0S</ExecutionTimeLimit></Settings><Actions Context=\"Author\"><Exec><Command>{}</Command><Arguments>start --home {}</Arguments><WorkingDirectory>{}</WorkingDirectory></Exec></Actions></Task>",
            xml(&exe.to_string_lossy()),
            xml(&format!("\"{}\"", home.display())),
            xml(&home.to_string_lossy())
        );
        let path = home.join("login-task.xml");
        config::write(&path, xml.as_bytes())?;
        run(
            "schtasks.exe",
            &[
                "/Create".into(),
                "/TN".into(),
                name,
                "/XML".into(),
                path.to_string_lossy().into_owned(),
                "/F".into(),
            ],
        )
        .await
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = (home, exe, enabled);
        bail!("login startup is not supported on this platform")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn quotes_systemd_arguments() {
        assert_eq!(systemd_quote("a b%$\n\"c"), "\"a b%%$$ \\\"c\"");
    }
}
