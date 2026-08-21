//! Blocking native dialogs used before the webview exists.

use std::process::Command;

#[cfg(test)]
thread_local! {
    static TEST_CHOICE: std::cell::Cell<Option<Choice>> = const { std::cell::Cell::new(None) };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Choice {
    Primary,
    Secondary,
    Escape,
}

pub(super) struct ChoiceSpec<'a> {
    pub title: &'a str,
    pub message: &'a str,
    pub primary: &'a str,
    pub secondary: Option<&'a str>,
    pub escape: &'a str,
}

pub(super) fn alert(title: &str, message: &str) {
    if cfg!(test)
        || (automation_enabled() && std::env::var_os("DSH_DESKTOP_DIALOG_DEFAULT").is_some())
    {
        eprintln!("dsh-desktop: native alert suppressed for automation: {title}: {message}");
        return;
    }
    let status = match std::env::consts::OS {
        "macos" => Command::new("osascript")
            .arg("-e")
            .arg(mac_alert_script(title, message))
            .status(),
        "windows" => windows_alert(title, message),
        _ => {
            let zenity = Command::new("zenity")
                .args([
                    "--warning",
                    "--title",
                    title,
                    "--text",
                    message,
                    "--width=520",
                ])
                .status();
            match zenity {
                Ok(status) if status.success() => Ok(status),
                _ => Command::new("notify-send").arg(title).arg(message).status(),
            }
        }
    };
    if let Err(error) = status {
        eprintln!("dsh-desktop: native alert failed: {error}");
    }
}

fn automation_enabled() -> bool {
    cfg!(debug_assertions) || std::env::var("DSH_DESKTOP_E2E_PROBE").ok().as_deref() == Some("1")
}

#[cfg(test)]
pub(super) fn with_test_choice<T>(choice: Choice, action: impl FnOnce() -> T) -> T {
    TEST_CHOICE.with(|slot| {
        let previous = slot.replace(Some(choice));
        let result = action();
        slot.set(previous);
        result
    })
}

/// Ask a blocking native question before the main window exists. Backend
/// failure always resolves to Escape so consent can never be inferred.
pub(super) fn choose(spec: &ChoiceSpec<'_>) -> Choice {
    #[cfg(test)]
    if let Some(choice) = TEST_CHOICE.with(std::cell::Cell::get) {
        return match choice {
            Choice::Secondary if spec.secondary.is_none() => Choice::Escape,
            choice => choice,
        };
    }
    if automation_enabled() {
        if let Ok(value) = std::env::var("DSH_DESKTOP_DIALOG_DEFAULT") {
            return match value.as_str() {
                "primary" => Choice::Primary,
                "secondary" if spec.secondary.is_some() => Choice::Secondary,
                "escape" => Choice::Escape,
                _ => Choice::Escape,
            };
        }
    }
    match std::env::consts::OS {
        "macos" => mac_choice(spec),
        "windows" => windows_choice(spec),
        _ => linux_choice(spec),
    }
}

fn mac_choice(spec: &ChoiceSpec<'_>) -> Choice {
    let output = Command::new("osascript")
        .arg("-e")
        .arg(mac_choice_script(spec))
        .output();
    let Ok(output) = output else {
        return Choice::Escape;
    };
    if !output.status.success() {
        return Choice::Escape;
    }
    let selected = String::from_utf8_lossy(&output.stdout);
    if selected.trim() == spec.primary {
        Choice::Primary
    } else if spec.secondary.is_some_and(|label| selected.trim() == label) {
        Choice::Secondary
    } else {
        Choice::Escape
    }
}

fn linux_choice(spec: &ChoiceSpec<'_>) -> Choice {
    let mut command = Command::new("zenity");
    command.args([
        "--question",
        "--title",
        spec.title,
        "--text",
        spec.message,
        "--ok-label",
        spec.primary,
        "--cancel-label",
        spec.escape,
        "--default-cancel",
        "--width=560",
    ]);
    if let Some(secondary) = spec.secondary {
        command.arg(format!("--extra-button={secondary}"));
    }
    let Ok(output) = command.output() else {
        return Choice::Escape;
    };
    linux_output_choice(output.status.success(), &output.stdout, spec.secondary)
}

fn linux_output_choice(success: bool, stdout: &[u8], secondary: Option<&str>) -> Choice {
    let selected = String::from_utf8_lossy(stdout);
    if secondary.is_some_and(|label| selected.trim() == label) {
        Choice::Secondary
    } else if success {
        Choice::Primary
    } else {
        Choice::Escape
    }
}

fn mac_alert_script(title: &str, message: &str) -> String {
    format!(
        "display alert \"{}\" message ({}) as warning",
        apple_escape(title, 160),
        apple_body(message)
    )
}

fn mac_choice_script(spec: &ChoiceSpec<'_>) -> String {
    let mut buttons = vec![spec.escape];
    if let Some(secondary) = spec.secondary {
        buttons.push(secondary);
    }
    buttons.push(spec.primary);
    let buttons = buttons
        .into_iter()
        .map(|label| format!("\"{}\"", apple_escape(label, 80)))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "set r to display alert \"{}\" message ({}) as warning buttons {{{}}} default button \"{}\" cancel button \"{}\"\nreturn button returned of r",
        apple_escape(spec.title, 160),
        apple_body(spec.message),
        buttons,
        apple_escape(spec.escape, 80),
        apple_escape(spec.escape, 80),
    )
}

fn apple_body(message: &str) -> String {
    let truncated = message.chars().take(1800).collect::<String>();
    let lines = truncated
        .lines()
        .map(|line| format!("\"{}\"", apple_escape(line, 900)))
        .collect::<Vec<_>>();
    if lines.is_empty() {
        "\"\"".to_string()
    } else {
        lines.join(" & return & ")
    }
}

fn apple_escape(value: &str, limit: usize) -> String {
    value
        .chars()
        .take(limit)
        .collect::<String>()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

#[cfg(windows)]
fn windows_alert(title: &str, message: &str) -> std::io::Result<std::process::ExitStatus> {
    let script = format!(
        "Add-Type -AssemblyName PresentationFramework; [System.Windows.MessageBox]::Show('{}','{}','OK','Warning') | Out-Null",
        powershell_escape(message),
        powershell_escape(title),
    );
    let encoded = super::utf16_le_base64(&script);
    let mut command = Command::new("powershell");
    command.args(["-NoProfile", "-NonInteractive", "-EncodedCommand", &encoded]);
    super::hide_console(&mut command);
    command.status()
}

#[cfg(windows)]
fn windows_choice(spec: &ChoiceSpec<'_>) -> Choice {
    let (buttons, default_button, legend) = match spec.secondary {
        Some(secondary) => (
            "YesNoCancel",
            "Cancel",
            format!(
                "{}\n\nYes / 是 = {}\nNo / 否 = {}\nCancel / 取消 = {}",
                spec.message, spec.primary, secondary, spec.escape
            ),
        ),
        None => (
            "YesNo",
            "No",
            format!(
                "{}\n\nYes / 是 = {}\nNo / 否 = {}",
                spec.message, spec.primary, spec.escape
            ),
        ),
    };
    let script = format!(
        "Add-Type -AssemblyName PresentationFramework; $r = [System.Windows.MessageBox]::Show('{}','{}','{}','Warning','{}'); if ($r -eq 'Yes') {{ exit 10 }} elseif ($r -eq 'No') {{ exit 11 }} else {{ exit 12 }}",
        powershell_escape(&legend),
        powershell_escape(spec.title),
        buttons,
        default_button,
    );
    let encoded = super::utf16_le_base64(&script);
    let mut command = Command::new("powershell");
    command.args(["-NoProfile", "-NonInteractive", "-EncodedCommand", &encoded]);
    super::hide_console(&mut command);
    match command.status().ok().and_then(|status| status.code()) {
        Some(10) => Choice::Primary,
        Some(11) if spec.secondary.is_some() => Choice::Secondary,
        _ => Choice::Escape,
    }
}

#[cfg(not(windows))]
fn windows_alert(_title: &str, _message: &str) -> std::io::Result<std::process::ExitStatus> {
    unreachable!("windows_alert is only called on Windows")
}

#[cfg(not(windows))]
fn windows_choice(_spec: &ChoiceSpec<'_>) -> Choice {
    unreachable!("windows_choice is only called on Windows")
}

#[cfg(windows)]
fn powershell_escape(value: &str) -> String {
    value
        .chars()
        .take(1800)
        .collect::<String>()
        .replace('\'', "''")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mac_choice_maps_safe_default_cancel_and_secondary_buttons() {
        let script = mac_choice_script(&ChoiceSpec {
            title: "Existing DSH home",
            message: "line one\nline \"two\"",
            primary: "Back up and continue",
            secondary: Some("Details"),
            escape: "Exit",
        });
        assert!(script.contains("buttons {\"Exit\", \"Details\", \"Back up and continue\"}"));
        assert!(script.contains("default button \"Exit\""));
        assert!(script.contains("cancel button \"Exit\""));
        assert!(script.contains("line \\\"two\\\""));
    }

    #[test]
    fn linux_extra_button_maps_to_secondary_even_with_exit_one() {
        assert_eq!(
            linux_output_choice(false, b"Details\n", Some("Details")),
            Choice::Secondary
        );
        assert_eq!(
            linux_output_choice(true, b"", Some("Details")),
            Choice::Primary
        );
        assert_eq!(
            linux_output_choice(false, b"", Some("Details")),
            Choice::Escape
        );
    }

    #[test]
    fn mac_choice_omits_secondary_button_when_unavailable() {
        let script = mac_choice_script(&ChoiceSpec {
            title: "Repair",
            message: "failed",
            primary: "Retry",
            secondary: None,
            escape: "Exit",
        });
        assert!(script.contains("buttons {\"Exit\", \"Retry\"}"));
        assert!(!script.contains("Details"));
    }
}
