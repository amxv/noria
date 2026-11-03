use anyhow::{Result, bail};
use std::process::{Command, Stdio};

fn command(program: &str) -> Command {
    let mut command = Command::new(program);
    command.stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    command
}

#[cfg(target_os = "linux")]
fn dialog(zenity: &[&str], kdialog: &[&str]) -> Result<Option<String>> {
    for (program, args) in [("zenity", zenity), ("kdialog", kdialog)] {
        match command(program).args(args).output() {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
            Ok(output) if output.status.success() => {
                return Ok(Some(
                    String::from_utf8_lossy(&output.stdout)
                        .trim_end_matches(['\r', '\n'])
                        .to_owned(),
                ));
            }
            Ok(output) if output.status.code() == Some(1) => return Ok(None),
            Ok(_) => bail!("desktop dialog could not open; check the graphical session"),
        }
    }
    bail!("install zenity (GNOME/Xfce/Cinnamon) or kdialog (KDE) for this action")
}

#[cfg(target_os = "linux")]
pub fn confirm(title: &str, body: &str) -> Result<bool> {
    Ok(dialog(
        &[
            "--question",
            "--title",
            title,
            "--text",
            body,
            "--no-wrap",
            "--no-markup",
        ],
        &["--yesno", body, "--title", title],
    )?
    .is_some())
}

#[cfg(target_os = "linux")]
pub fn prompt(title: &str, initial: &str) -> Result<Option<String>> {
    dialog(
        &[
            "--entry",
            "--title",
            title,
            "--text",
            title,
            "--entry-text",
            initial,
        ],
        &["--inputbox", title, initial, "--title", title],
    )
}

#[cfg(target_os = "linux")]
pub fn choose_folder() -> Result<Option<String>> {
    dialog(
        &[
            "--file-selection",
            "--directory",
            "--title",
            "Select client configuration folder",
        ],
        &[
            "--getexistingdirectory",
            ".",
            "--title",
            "Select client configuration folder",
        ],
    )
}

#[cfg(target_os = "linux")]
pub fn choose_provider() -> Result<Option<String>> {
    let value = dialog(
        &[
            "--list",
            "--radiolist",
            "--title",
            "Choose client",
            "--column",
            "Select",
            "--column",
            "Client",
            "TRUE",
            "codex",
            "FALSE",
            "claude",
        ],
        &[
            "--radiolist",
            "Choose client",
            "codex",
            "codex",
            "on",
            "claude",
            "claude",
            "off",
        ],
    )?;
    Ok(value.filter(|value| value == "codex" || value == "claude"))
}

#[cfg(target_os = "linux")]
pub fn message(title: &str, body: &str) {
    let _ = dialog(
        &[
            "--info",
            "--title",
            title,
            "--text",
            body,
            "--no-wrap",
            "--no-markup",
        ],
        &["--msgbox", body, "--title", title],
    );
}

#[cfg(target_os = "linux")]
pub fn notice(title: &str, body: &str) {
    if !command("notify-send")
        .args(["--app-name=Noria", title, body])
        .status()
        .is_ok_and(|status| status.success())
    {
        message(title, body);
    }
}

#[cfg(target_os = "linux")]
pub fn open_url(address: &str) -> Result<()> {
    let mut child = command("xdg-open")
        .arg(address)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    // Reap the launcher without blocking the UI event loop.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(windows)]
fn show_box(title: &str, body: &str, flags: u32) -> Result<i32> {
    let title = title.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
    let body = body.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
    let result = unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::MessageBoxW(
            std::ptr::null_mut(),
            body.as_ptr(),
            title.as_ptr(),
            flags,
        )
    };
    if result == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(result)
}

#[cfg(windows)]
pub fn confirm(title: &str, body: &str) -> Result<bool> {
    Ok(show_box(title, body, 0x24)? == 6)
}

#[cfg(windows)]
pub fn message(title: &str, body: &str) {
    let _ = show_box(title, body, 0x40);
}

#[cfg(windows)]
pub fn notice(title: &str, body: &str) {
    message(title, body);
}

#[cfg(windows)]
fn ps_string(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(windows)]
fn powershell(script: &str) -> Result<Option<String>> {
    use base64::Engine;
    let script =
        format!("[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false);\n{script}");
    let bytes = script
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    let result = command("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-STA",
            "-EncodedCommand",
            &encoded,
        ])
        .output()?;
    if !result.status.success() {
        bail!("Windows input dialog failed");
    }
    let text = String::from_utf8_lossy(&result.stdout)
        .trim_end_matches(['\r', '\n'])
        .to_owned();
    Ok((text != "__NORIA_CANCEL__").then_some(text))
}

#[cfg(windows)]
pub fn prompt(title: &str, initial: &str) -> Result<Option<String>> {
    powershell(&format!(
        r#"Add-Type -AssemblyName System.Windows.Forms;
$f = New-Object System.Windows.Forms.Form; $f.Text={}; $f.Width=420; $f.Height=160; $f.StartPosition='CenterScreen'; $f.TopMost=$true;
$input = New-Object System.Windows.Forms.TextBox; $input.SetBounds(15,15,375,25); $input.Text={}; $f.Controls.Add($input);
$ok = New-Object System.Windows.Forms.Button; $ok.Text='Save'; $ok.SetBounds(220,60,80,30); $ok.DialogResult='OK'; $f.Controls.Add($ok); $f.AcceptButton=$ok;
$cancel = New-Object System.Windows.Forms.Button; $cancel.Text='Cancel'; $cancel.SetBounds(310,60,80,30); $cancel.DialogResult='Cancel'; $f.Controls.Add($cancel); $f.CancelButton=$cancel;
if ($f.ShowDialog() -eq 'OK') {{ [Console]::WriteLine($input.Text) }} else {{ [Console]::WriteLine('__NORIA_CANCEL__') }}; $f.Dispose()"#,
        ps_string(title),
        ps_string(initial)
    ))
}

#[cfg(windows)]
pub fn choose_folder() -> Result<Option<String>> {
    powershell(
        r#"Add-Type -AssemblyName System.Windows.Forms;
$f = New-Object System.Windows.Forms.FolderBrowserDialog; $f.Description='Select Codex or Claude Code configuration folder';
if ($f.ShowDialog() -eq 'OK') { [Console]::WriteLine($f.SelectedPath) } else { [Console]::WriteLine('__NORIA_CANCEL__') }; $f.Dispose()"#,
    )
}

#[cfg(windows)]
pub fn choose_provider() -> Result<Option<String>> {
    match show_box(
        "Choose client",
        "Configure this folder for Codex (Yes) or Claude Code (No)?",
        0x23,
    )? {
        6 => Ok(Some("codex".into())),
        7 => Ok(Some("claude".into())),
        _ => Ok(None),
    }
}

#[cfg(windows)]
pub fn open_url(address: &str) -> Result<()> {
    let mut child = command("rundll32.exe")
        .args(["url.dll,FileProtocolHandler", address])
        .spawn()?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}
