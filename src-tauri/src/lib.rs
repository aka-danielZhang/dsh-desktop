//! Desktop shell for the DeepSeek Harness web UI (M1).
//!
//! One run: find the harness checkout, idempotently install the
//! desktop-owned plugins into the web profile, spawn `dsh web` on a random
//! loopback port with the user's real `~/.dsh` as DSH_HOME (the desktop IS
//! another face of the same account), poll `GET /` until ready, then open the
//! main window with the desktop gate signal injected. Sidecar output is teed
//! to a per-boot `desktop-<timestamp>.log` under the shared harness log
//! directory (the `web:log` convention). IPC command backends implement the
//! contract table in the repository AGENTS.md.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tauri::{Manager, RunEvent, WebviewUrl, WebviewWindowBuilder};

mod native_dialog;
mod profile_adoption;
mod profile_repair;
#[cfg(windows)]
mod win;

const BRIDGE_PACKAGE: &str = "dsh-desktop-bridge";
const COMPACTION_PACKAGE: &str = "dsh-compaction-hierarchical";
const MISSING_RESTORE_SOURCE: &str = "[missing-web-profile]";
const COMPACTION_RUNTIME_PEERS: &[&str] = &[
    "@deepseek-ai/cordis",
    "@deepseek-ai/dsh-agent",
    "@deepseek-ai/dsh-compaction-basic",
    "@deepseek-ai/dsh-llm",
    "@deepseek-ai/dsh-token-meter",
    "@deepseek-ai/schemastery",
];

/// Ready-probe cadence and budget (tsx cold start is slow).
const PROBE_INTERVAL: Duration = Duration::from_millis(500);
const PROBE_BUDGET: Duration = Duration::from_secs(120);

/// Grace period between SIGTERM and SIGKILL in the termination ladder.
const TERM_GRACE: Duration = Duration::from_secs(3);
/// Termination-ladder poll cadence.
const LADDER_TICK: Duration = Duration::from_millis(100);

/// The sidecar child, terminated through the SIGTERM→SIGKILL ladder when
/// the app exits or catches a termination signal.
static SIDECAR: Mutex<Option<Child>> = Mutex::new(None);

/// The stale-sidecar registry path for this process (set once during boot).
static REGISTRY: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// The termination signal caught by the handler, published for the poller
/// thread (an atomic store is all the handler itself is allowed to do).
#[cfg(unix)]
static SIGNALED: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

/// The e2e verdict reported through the IPC channel, if it works.
static E2E_VERDICT: Mutex<Option<String>> = Mutex::new(None);

/// Process-wide updater state consumed by the plugin's title-band control.
/// Tauri streams chunk lengths through callbacks; keeping the accumulator here
/// lets the control attach midway through a download.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
#[serde(tag = "phase", rename_all = "camelCase")]
enum DesktopUpdateStatus {
    Idle,
    Checking,
    Current,
    Available {
        version: String,
        notes: String,
    },
    Preparing {
        version: Option<String>,
    },
    Downloading {
        version: String,
        downloaded: u64,
        total: Option<u64>,
    },
    Ready {
        version: String,
    },
    Installing {
        version: String,
    },
    Restarting {
        version: String,
    },
    Failed {
        #[serde(skip_serializing_if = "Option::is_none")]
        version: Option<String>,
        message: String,
    },
}

impl DesktopUpdateStatus {
    /// A network/install operation owns the updater while these phases hold.
    fn is_busy(&self) -> bool {
        matches!(
            self,
            Self::Checking
                | Self::Preparing { .. }
                | Self::Downloading { .. }
                | Self::Installing { .. }
                | Self::Restarting { .. }
        )
    }

    /// Preserve the target version when an in-flight operation fails.
    fn version(&self) -> Option<String> {
        match self {
            Self::Available { version, .. }
            | Self::Downloading { version, .. }
            | Self::Ready { version }
            | Self::Installing { version }
            | Self::Restarting { version } => Some(version.clone()),
            Self::Preparing { version } | Self::Failed { version, .. } => version.clone(),
            Self::Idle | Self::Checking | Self::Current => None,
        }
    }
}

static UPDATE_STATUS: Mutex<DesktopUpdateStatus> = Mutex::new(DesktopUpdateStatus::Idle);

/// Signature-verified package retained only until the user confirms install.
struct DownloadedUpdate {
    update: tauri_plugin_updater::Update,
    bytes: Vec<u8>,
}

static DOWNLOADED_UPDATE: Mutex<Option<DownloadedUpdate>> = Mutex::new(None);

/// Run the shell.
pub fn run() {
    #[cfg(unix)]
    install_terminate_signals();
    tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(|app| {
            let app_handle = app.handle().clone();
            std::thread::spawn(move || match boot_sequence(&app_handle) {
                Ok(BootOutcome::Started) => {}
                Ok(BootOutcome::ExitRequested) => app_handle.exit(0),
                Err(error) => {
                    eprintln!("dsh-desktop: boot failed: {error}");
                    native_dialog::alert("无法启动 DeepSeek Harness", &error);
                    app_handle.exit(1);
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            dsh_desktop_open_external,
            dsh_desktop_notify,
            dsh_desktop_save_file,
            dsh_desktop_e2e_report,
            dsh_desktop_check_update,
            dsh_desktop_update_status,
            dsh_desktop_download_update,
            dsh_desktop_install_update
        ])
        .build(tauri::generate_context!())
        .expect("dsh-desktop: tauri context")
        .run(|_app, event| {
            match event {
                RunEvent::Exit => kill_sidecar(),
                RunEvent::ExitRequested { code: Some(code), .. } => {
                    kill_sidecar();
                    std::process::exit(code);
                }
                _ => {}
            }
        });
}

/// Terminate the sidecar child if one is running (idempotent): SIGTERM
/// first so the harness can flush, SIGKILL after the grace period, then
/// drop our registry entry so the next boot does not reap a dead pid.
fn kill_sidecar() {
    let Some(mut child) = SIDECAR.lock().ok().and_then(|mut guard| guard.take()) else {
        return;
    };
    let pid = child.id();
    #[cfg(unix)]
    {
        let target = signal_target(pid);
        unsafe { libc::kill(target, libc::SIGTERM) };
        let deadline = Instant::now() + TERM_GRACE;
        while Instant::now() < deadline {
            match child.try_wait() {
                Ok(Some(_)) | Err(_) => break,
                Ok(None) => std::thread::sleep(LADDER_TICK),
            }
        }
        if matches!(child.try_wait(), Ok(None)) {
            unsafe { libc::kill(target, libc::SIGKILL) };
        }
        let _ = child.wait();
        // Grandchildren that ignored SIGTERM while the leader exited early:
        // one last group sweep. An empty group is a harmless ESRCH no-op.
        unsafe { libc::kill(target, libc::SIGKILL) };
    }
    #[cfg(windows)]
    {
        crate::win::terminate_job();
        crate::win::taskkill_tree(pid, true);
        let _ = child.wait();
    }
    unregister_sidecar(pid);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BootOutcome {
    Started,
    ExitRequested,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AdoptionPlan {
    Resume,
    StartFresh,
    AskExisting,
}

fn plan_profile_adoption(
    has_existing_data: bool,
    previous_status: Option<profile_adoption::AdoptionStatus>,
) -> AdoptionPlan {
    match previous_status {
        Some(
            profile_adoption::AdoptionStatus::ConsentRequired
            | profile_adoption::AdoptionStatus::Restored
            | profile_adoption::AdoptionStatus::RestoreAbandoned,
        ) => AdoptionPlan::AskExisting,
        Some(_) => AdoptionPlan::Resume,
        None if has_existing_data => AdoptionPlan::AskExisting,
        None => AdoptionPlan::StartFresh,
    }
}

/// Boot to a ready window: shared-home consent, profile repair, sidecar spawn,
/// readiness, and window creation. No sidecar starts while consent or repair
/// is unresolved.
fn boot_sequence(app: &tauri::AppHandle) -> Result<BootOutcome, String> {
    let shell_root = shell_root()?;
    let dsh_home = dsh_home()?;
    profile_repair::recover_web_profile(&dsh_home)?;
    let summary = profile_adoption::inspect_home(&dsh_home)?;
    let mut adoption = match prepare_profile_adoption(&shell_root, &summary)? {
        Some(record) => record,
        None => return Ok(BootOutcome::ExitRequested),
    };

    let runtime = find_runtime(app)?;
    let bridge = find_bridge(app)?;
    let compaction = find_compaction_plugin(app)?;

    // Reap sidecars orphaned by a previous shell before adding our own:
    // a `tauri dev` watcher restart SIGKILLs the app outright (see the
    // supervision section below), so the previous boot's exit path never
    // ran and its sidecar is still out there holding a port and ~/.dsh.
    {
        let registry = registry_path(&shell_root);
        let _ = REGISTRY.set(registry.clone());
        for entry in sweep_stale_sidecars(&registry) {
            println!(
                "dsh-desktop: reaped stale sidecar pid={} port={} (shell pid {} is gone; log {})",
                entry.sidecar_pid, entry.port, entry.shell_pid, entry.log
            );
        }
    }

    let plugins = [
        (bridge.as_path(), BRIDGE_PACKAGE),
        (compaction.as_path(), COMPACTION_PACKAGE),
    ];
    let outcome = install_with_profile_repair(
        &runtime,
        &plugins,
        &dsh_home,
        &shell_root,
        &summary.canonical_home,
        &mut adoption,
    )?;
    if outcome == BootOutcome::ExitRequested {
        return Ok(outcome);
    }

    // Add runtime-owned peers only after profile installation so pnpm never
    // packs managed links into the profile store.
    ensure_bridge_cordis_link(&bridge, &runtime);
    ensure_bundled_plugin_runtime_links(&compaction, &runtime, COMPACTION_RUNTIME_PEERS)?;

    let port = free_port()?;
    let url = format!("http://127.0.0.1:{port}");
    let sidecar_log = spawn_sidecar(&runtime, &dsh_home, port)?;

    if !wait_ready(port) {
        return Err(format!(
            "harness server at {url} did not answer GET / within {}s (see {})",
            PROBE_BUDGET.as_secs(),
            sidecar_log.display()
        ));
    }

    let e2e = std::env::var("DSH_DESKTOP_E2E_PROBE").ok().as_deref() == Some("1");
    open_main_window(app, &url, e2e)?;
    Ok(BootOutcome::Started)
}

fn prepare_profile_adoption(
    shell_root: &Path,
    summary: &profile_adoption::ExistingHomeSummary,
) -> Result<Option<profile_adoption::AdoptionRecord>, String> {
    profile_adoption::cleanup_stale_backup_staging(shell_root, &summary.canonical_home)?;
    let previous = profile_adoption::latest_record(shell_root, &summary.canonical_home)?;
    if let Some(record) = previous.as_ref() {
        if let Some(backup) = record.backup.as_ref() {
            if let Err(error) =
                profile_adoption::verify_backup(shell_root, &summary.canonical_home, backup)
            {
                loop {
                    match native_dialog::choose(&native_dialog::ChoiceSpec {
                        title: "Web Profile 备份校验失败",
                        message: &format!(
                            "Desktop 不会静默使用或删除这份备份。\n\n原因：{error}\n\n你可以保留当前 Web Profile 并撤销旧恢复点，查看备份位置，或退出。"
                        ),
                        primary: "保留当前 Profile",
                        secondary: Some("查看备份位置"),
                        escape: "退出",
                    }) {
                        native_dialog::Choice::Primary => {
                            profile_adoption::transition(
                                shell_root,
                                record,
                                profile_adoption::AdoptionStatus::ConsentRequired,
                                None,
                            )?;
                            native_dialog::alert(
                                "已保留当前 Web Profile",
                                "旧恢复点已从 Desktop 的活动状态中移除；若此前正在恢复，该恢复请求也已撤销。磁盘上的备份文件没有被删除。Desktop 现在将退出；下次启动会重新征求授权并创建新备份。",
                            );
                            return Ok(None);
                        }
                        native_dialog::Choice::Secondary => native_dialog::alert(
                            "备份位置",
                            &format!("{}\n\n校验错误：{error}", backup.root.display()),
                        ),
                        native_dialog::Choice::Escape => return Ok(None),
                    }
                }
            }
        }
    }

    match plan_profile_adoption(
        summary.has_existing_data,
        previous.as_ref().map(|record| record.status),
    ) {
        AdoptionPlan::Resume => return Ok(previous),
        AdoptionPlan::StartFresh => {
            return profile_adoption::start_record(
                shell_root,
                &summary.canonical_home,
                profile_adoption::AdoptionOrigin::FreshHome,
                false,
                None,
            )
            .map(Some);
        }
        AdoptionPlan::AskExisting => {}
    }

    loop {
        let backup_note = if summary.has_web_profile {
            "继续前会保存一份可恢复的当前 Web Profile 配置快照。"
        } else {
            "当前没有 Web Profile，因此没有需要备份的 Profile；Desktop 会新建它。"
        };
        let primary = if summary.has_web_profile {
            "备份并继续"
        } else {
            "继续"
        };
        let message = format!(
            "检测到现有 DSH 数据目录：{}\n\n其中有 {} 个 Web Profile 插件、{} 个 Agent 预设。Desktop 与终端 DSH 将共享该目录。\n\n继续后只会更新 Web Profile，添加或刷新 {} 和 {}。现有会话、凭据、设置、Agent 预设、其他 Profile 与其他插件都会保留。{}",
            summary.canonical_home.display(),
            summary.plugins.len(),
            summary.agent_preset_count,
            BRIDGE_PACKAGE,
            COMPACTION_PACKAGE,
            backup_note,
        );
        match native_dialog::choose(&native_dialog::ChoiceSpec {
            title: "使用现有的 DSH 数据？",
            message: &message,
            primary,
            secondary: Some("查看变更"),
            escape: "退出",
        }) {
            native_dialog::Choice::Primary => {
                let backup = if summary.has_web_profile {
                    Some(profile_adoption::create_backup(
                        shell_root,
                        &summary.canonical_home,
                    )?)
                } else {
                    None
                };
                let record = if let Some(previous) = previous.as_ref() {
                    profile_adoption::restart_with_consent(shell_root, previous, backup)?
                } else {
                    profile_adoption::start_record(
                        shell_root,
                        &summary.canonical_home,
                        profile_adoption::AdoptionOrigin::ExistingHome,
                        true,
                        backup,
                    )?
                };
                return Ok(Some(record));
            }
            native_dialog::Choice::Secondary => {
                let plugins = if summary.plugins.is_empty() {
                    "（当前 Web Profile 没有声明插件）".to_string()
                } else {
                    summary.plugins.join("\n- ")
                };
                native_dialog::alert(
                    "Desktop 将修改的范围",
                    &format!(
                        "DSH_HOME：{}\nWeb Profile：{}/profiles/web\n\n现有插件：\n- {}\n\nDesktop 只更新这个 Web Profile 的 package manifest、lockfile 与 node_modules，并新增或刷新两个 desktop-owned 包。\n\n不会修改 sessions、credentials、settings、.agent-presets、home cordis.patch.yml 或其他 profiles。",
                        summary.canonical_home.display(),
                        summary.canonical_home.display(),
                        plugins,
                    ),
                );
            }
            native_dialog::Choice::Escape => return Ok(None),
        }
    }
}

fn install_with_profile_repair(
    runtime: &Runtime,
    plugins: &[(&Path, &str)],
    dsh_home: &Path,
    shell_root: &Path,
    canonical_home: &Path,
    adoption: &mut profile_adoption::AdoptionRecord,
) -> Result<BootOutcome, String> {
    loop {
        let attempt = if adoption.status == profile_adoption::AdoptionStatus::RestorePending {
            restore_adoption_backup(runtime, dsh_home, shell_root, canonical_home, adoption).map(
                |record| {
                    *adoption = record;
                    BootOutcome::ExitRequested
                },
            )
        } else {
            let expectation = if adoption.status == profile_adoption::AdoptionStatus::Adopting {
                adoption.backup.as_ref().map_or(
                    profile_repair::ProfileExpectation::Missing,
                    |backup| {
                        profile_repair::ProfileExpectation::Identity(
                            backup.source_identity.as_str(),
                        )
                    },
                )
            } else {
                profile_repair::ProfileExpectation::Unchecked
            };
            run_desktop_plugin_install(runtime, plugins, dsh_home, shell_root, expectation)
                .and_then(|()| {
                    if adoption.status == profile_adoption::AdoptionStatus::Adopting {
                        let active = profile_adoption::transition(
                            shell_root,
                            adoption,
                            profile_adoption::AdoptionStatus::Active,
                            adoption.backup.clone(),
                        )?;
                        *adoption = active;
                        if let Some(backup) = adoption.backup.as_ref() {
                            if let Err(error) = profile_adoption::prune_other_backups(
                                shell_root,
                                canonical_home,
                                backup,
                            ) {
                                eprintln!("dsh-desktop: prune old profile backups failed: {error}");
                            }
                        }
                    }
                    Ok(BootOutcome::Started)
                })
        };

        let error = match attempt {
            Ok(outcome) => return Ok(outcome),
            Err(error) => error,
        };
        let pending_restore = adoption.status == profile_adoption::AdoptionStatus::RestorePending;
        let expectation_mismatch = profile_repair::is_expectation_mismatch(&error);
        if expectation_mismatch
            && adoption.status == profile_adoption::AdoptionStatus::Adopting
            && adoption.backup.is_none()
        {
            *adoption = profile_adoption::transition(
                shell_root,
                adoption,
                profile_adoption::AdoptionStatus::ConsentRequired,
                None,
            )?;
            native_dialog::alert(
                "检测到新的 Web Profile",
                "在 Desktop 检查到空 Home 之后，终端创建或修改了 Web Profile。Desktop 没有修改它，也不会把之前的空 Home 判断当作授权。\n\nDesktop 现在将退出；下次启动会重新确认共享范围并先保存这个新 Profile。",
            );
            return Ok(BootOutcome::ExitRequested);
        }
        if expectation_mismatch && !pending_restore && adoption.backup.is_some() {
            match native_dialog::choose(&native_dialog::ChoiceSpec {
                title: "Web Profile 已发生变化",
                message: "备份之后，Web Profile 又发生了变化；这也可能表示上一次 Desktop 事务已提交、但状态尚未收尾。Desktop 尚未执行新的覆盖。\n\n你可以先保存当前状态再继续，恢复已保存备份，或退出。",
                primary: "保存当前状态并继续",
                secondary: Some("恢复已保存备份"),
                escape: "退出",
            }) {
                native_dialog::Choice::Primary => {
                    refresh_adoption_backup_if_needed(shell_root, canonical_home, adoption)?;
                    continue;
                }
                native_dialog::Choice::Secondary => {
                    let source_identity = current_restore_source(canonical_home)?;
                    *adoption =
                        profile_adoption::begin_restore(shell_root, adoption, source_identity)?;
                    continue;
                }
                native_dialog::Choice::Escape => return Ok(BootOutcome::ExitRequested),
            }
        }
        if expectation_mismatch && pending_restore {
            let action = native_dialog::choose(&native_dialog::ChoiceSpec {
                title: "终端已修改 Web Profile",
                message: "恢复请求之后，终端又修改了 Web Profile。Desktop 已停止恢复，不会用旧备份覆盖这些新修改。\n\n你可以保留当前 Profile 并撤销这次恢复请求，或直接退出。",
                primary: "保留当前 Profile",
                secondary: None,
                escape: "退出",
            });
            if action == native_dialog::Choice::Primary {
                *adoption = profile_adoption::transition(
                    shell_root,
                    adoption,
                    profile_adoption::AdoptionStatus::RestoreAbandoned,
                    adoption.backup.clone(),
                )?;
                native_dialog::alert(
                    "已保留当前 Web Profile",
                    "这次恢复请求已撤销。Desktop 现在将退出；下次启动会重新征求共享 DSH_HOME 的授权。",
                );
            }
            return Ok(BootOutcome::ExitRequested);
        }
        let can_restore = !pending_restore && adoption.backup.is_some();
        let secondary = if pending_restore {
            Some("保留当前 Profile")
        } else {
            can_restore.then_some("恢复已保存备份")
        };
        let action = native_dialog::choose(&native_dialog::ChoiceSpec {
            title: if pending_restore {
                "Web Profile 恢复未完成"
            } else {
                "Web Profile 更新未完成"
            },
            message: &format!(
                "Desktop 尚未启动 sidecar，真实 Web Profile 没有被部分覆盖。\n\n原因：{error}\n\n安装日志：{}\n\n你可以重试{}，或者退出后继续使用终端 DSH。",
                shell_root.join("logs/install.log").display(),
                if pending_restore {
                    "、保留当前 Profile 并撤销恢复"
                } else if can_restore {
                    "、恢复已保存备份"
                } else {
                    ""
                },
            ),
            primary: "重试",
            secondary,
            escape: "退出",
        });
        match action {
            native_dialog::Choice::Primary => {}
            native_dialog::Choice::Secondary if pending_restore => {
                *adoption = profile_adoption::transition(
                    shell_root,
                    adoption,
                    profile_adoption::AdoptionStatus::RestoreAbandoned,
                    adoption.backup.clone(),
                )?;
                native_dialog::alert(
                    "已保留当前 Web Profile",
                    "这次恢复请求已撤销。Desktop 现在将退出；下次启动会重新征求共享 DSH_HOME 的授权。",
                );
                return Ok(BootOutcome::ExitRequested);
            }
            native_dialog::Choice::Secondary if can_restore => {
                let source_identity = current_restore_source(canonical_home)?;
                *adoption = profile_adoption::begin_restore(shell_root, adoption, source_identity)?;
            }
            _ => return Ok(BootOutcome::ExitRequested),
        }
    }
}

fn current_restore_source(canonical_home: &Path) -> Result<String, String> {
    Ok(profile_repair::web_profile_identity(canonical_home)?
        .unwrap_or_else(|| MISSING_RESTORE_SOURCE.to_string()))
}

fn refresh_adoption_backup_if_needed(
    shell_root: &Path,
    canonical_home: &Path,
    adoption: &mut profile_adoption::AdoptionRecord,
) -> Result<(), String> {
    let Some(existing) = adoption.backup.as_ref() else {
        return Ok(());
    };
    let current = profile_repair::web_profile_identity(canonical_home)?;
    if current.as_deref() == Some(existing.source_identity.as_str()) {
        return Ok(());
    }
    let backup = profile_adoption::create_backup(shell_root, canonical_home)?;
    *adoption = profile_adoption::transition(
        shell_root,
        adoption,
        profile_adoption::AdoptionStatus::Adopting,
        Some(backup),
    )?;
    Ok(())
}

fn restore_adoption_backup(
    runtime: &Runtime,
    dsh_home: &Path,
    shell_root: &Path,
    canonical_home: &Path,
    adoption: &profile_adoption::AdoptionRecord,
) -> Result<profile_adoption::AdoptionRecord, String> {
    let backup = adoption
        .backup
        .as_ref()
        .ok_or_else(|| "no pre-adoption Web Profile backup is available".to_string())?;
    profile_adoption::verify_backup(shell_root, canonical_home, backup)?;
    if !profile_adoption::current_profile_matches_backup(canonical_home, backup)? {
        let expected = adoption
            .restore_source_identity
            .as_deref()
            .ok_or_else(|| "pending profile restore has no source identity".to_string())?;
        let expectation = if profile_repair::web_profile_identity(canonical_home)?.is_none() {
            profile_repair::ProfileExpectation::Missing
        } else {
            profile_repair::ProfileExpectation::Identity(expected)
        };
        profile_repair::mutate_web_profile_expected(
            dsh_home,
            &[],
            expectation,
            |shadow_home, _| {
                let profile = shadow_home.join("profiles/web");
                profile_repair::replace_profile_from_snapshot(&backup.profile, &profile)?;
                if !profile.join("pnpm-lock.yaml").is_file() {
                    return Err(
                        "profile backup has no pnpm-lock.yaml for a frozen restore".to_string()
                    );
                }
                frozen_profile_install_once(runtime, shadow_home, shell_root)?;
                let restored = profile_repair::profile_snapshot_identity(&profile)?;
                if restored != backup.snapshot_identity {
                    return Err(
                        "frozen restore changed the backed-up Web Profile configuration"
                            .to_string(),
                    );
                }
                validate_profile_config(runtime, shadow_home, shell_root)
            },
        )?;
    }
    if !profile_adoption::current_profile_matches_backup(canonical_home, backup)? {
        return Err("restored Web Profile does not match the approved backup".to_string());
    }
    let restored = profile_adoption::transition(
        shell_root,
        adoption,
        profile_adoption::AdoptionStatus::Restored,
        Some(backup.clone()),
    )?;
    native_dialog::alert(
        "Web Profile 已恢复",
        &format!(
            "已恢复到你确认共享 DSH_HOME 时保存的 Web Profile 配置。\n\n备份：{}\n\nDesktop 现在将退出；下次启动会重新征求共享 DSH_HOME 的授权。",
            profile_adoption::backup_details(backup),
        ),
    );
    Ok(restored)
}

/// Locate the harness checkout (dev source fallback only — runtime/build and
/// the release extraction take precedence in find_runtime): $DSH_CHECKOUT,
/// then the sibling checkout beside this repo, then the conventional path —
/// the same candidate order as scripts/setup-plugins.mjs.
fn find_checkout() -> Result<PathBuf, String> {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(from_env) = std::env::var("DSH_CHECKOUT") {
        candidates.push(PathBuf::from(from_env));
    }
    candidates.push(repo_root.join("../deepseek-harness"));
    if let Ok(home) = user_home() {
        candidates.push(home.join("workspace/deepseek-harness"));
    }
    for candidate in &candidates {
        if candidate.join("docs/architecture.md").is_file() && candidate.join("apps/cli/src/bin.ts").is_file() {
            return Ok(candidate.clone());
        }
    }
    Err(format!(
        "no DeepSeek Harness checkout found (need docs/architecture.md and apps/cli/src/bin.ts); tried: {}",
        candidates.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
    ))
}

/// How the sidecar is launched: a prebuilt runtime tree, or the source checkout.
struct Runtime {
    /// Node binary (bundled tools or PATH).
    node: PathBuf,
    /// Extra args before the CLI entry (source mode: ["--import", "tsx/esm"]).
    args_prefix: Vec<String>,
    /// The dsh CLI entry (bundled: lib/bin.js; source: apps/cli/src/bin.ts).
    cli: PathBuf,
    /// Working directory for the CLI.
    cwd: PathBuf,
    /// Directories prepended to PATH so the CLI finds pnpm and tools.
    path_prepend: Vec<PathBuf>,
}

/// Build the common Node + DSH CLI invocation without choosing a PATH policy.
fn base_cli_command(runtime: &Runtime) -> Command {
    let mut command = Command::new(&runtime.node);
    for arg in &runtime.args_prefix {
        command.arg(arg);
    }
    command.arg(&runtime.cli);
    command.current_dir(&runtime.cwd);
    hide_console(&mut command);
    command
}

/// Build a profile-management CLI invocation. Runtime-owned tools precede
/// inherited PATH, and no desktop-discovered host directories are injected.
fn cli_command(runtime: &Runtime) -> Command {
    let mut command = base_cli_command(runtime);
    set_process_path(&mut command, &runtime.path_prepend);
    command
}

/// Build the long-lived web sidecar invocation. Runtime tools still win, then
/// common host CLI locations fill the PATH Finder/Dock and desktop launchers
/// omit, and finally the inherited PATH remains available.
fn sidecar_command(runtime: &Runtime) -> Command {
    sidecar_command_with_host_dirs(runtime, &host_cli_path_dirs())
}

fn sidecar_command_with_host_dirs(runtime: &Runtime, host_dirs: &[PathBuf]) -> Command {
    let mut command = base_cli_command(runtime);
    let mut preferred = runtime.path_prepend.clone();
    preferred.extend_from_slice(host_dirs);
    set_process_path(&mut command, &preferred);
    command
}

fn set_process_path(command: &mut Command, preferred: &[PathBuf]) {
    let inherited = std::env::var_os("PATH");
    match compose_process_path(preferred, inherited.as_deref()) {
        Ok(path) => {
            command.env("PATH", path);
        }
        Err(error) => {
            eprintln!("dsh-desktop: keep inherited PATH ({error})");
        }
    }
}

/// Compose PATH with platform-aware parsing/joining and stable de-duplication.
fn compose_process_path(preferred: &[PathBuf], inherited: Option<&OsStr>) -> Result<OsString, String> {
    let mut paths = Vec::<PathBuf>::new();
    for path in preferred.iter().cloned().chain(inherited.into_iter().flat_map(std::env::split_paths)) {
        if path.as_os_str().is_empty() || paths.iter().any(|existing| existing == &path) {
            continue;
        }
        paths.push(path);
    }
    std::env::join_paths(paths).map_err(|error| format!("compose process PATH: {error}"))
}

/// Existing host CLI directories that GUI-launched apps commonly miss.
/// These are sidecar-only: profile management keeps the runtime PATH above.
fn host_cli_path_dirs() -> Vec<PathBuf> {
    let mut candidates = Vec::<PathBuf>::new();
    if let Ok(home) = user_home() {
        candidates.push(home.join(".local/bin"));
        candidates.push(home.join(".bun/bin"));
        candidates.push(home.join(".cargo/bin"));
        #[cfg(target_os = "macos")]
        {
            candidates.push(home.join("Library/pnpm"));
            candidates.push(home.join(".npm-global/bin"));
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        candidates.push(home.join(".linuxbrew/bin"));
        #[cfg(windows)]
        candidates.push(home.join(r"scoop\shims"));
    }
    #[cfg(target_os = "macos")]
    {
        candidates.push(PathBuf::from("/opt/homebrew/bin"));
        candidates.push(PathBuf::from("/opt/homebrew/sbin"));
        candidates.push(PathBuf::from("/usr/local/bin"));
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        candidates.push(PathBuf::from("/usr/local/bin"));
        candidates.push(PathBuf::from("/home/linuxbrew/.linuxbrew/bin"));
    }
    #[cfg(windows)]
    {
        if let Ok(app_data) = std::env::var("APPDATA") {
            candidates.push(PathBuf::from(app_data).join("npm"));
        }
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            candidates.push(PathBuf::from(&local).join("pnpm"));
            candidates.push(PathBuf::from(local).join(r"Programs\Microsoft VS Code\bin"));
        }
        if let Ok(program_files) = std::env::var("ProgramFiles") {
            candidates.push(PathBuf::from(program_files).join("nodejs"));
        }
    }
    candidates.retain(|path| path.is_dir());
    candidates
}

/// Keep child consoles off the desktop. Windows `node.exe` is a console
/// subsystem binary; without this flag every sidecar/plugin-install spawn
/// would flash (or leave) a cmd window. No-op on Unix.
pub(crate) fn hide_console(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let _ = command;
}

/// Resolve the sidecar runtime: $DSH_DESKTOP_RUNTIME, then the repo's own
/// runtime/build/<sha> from runtime/revision.json, then the release bundle's
/// resources (extracted to ~/.dsh-desktop on first boot), then the source
/// checkout (dev fallback).
///
/// runtime/build precedes the extraction: `tauri dev` runs the bare debug
/// binary against the repo, and a ~/.dsh-desktop extraction can belong to an
/// INSTALLED app of a different (older) revision — dev would silently boot
/// stale code. The installed app always wins in production anyway: its
/// bundled resources exist and the repo checkout (with runtime/build) does
/// not ship inside the .app.
fn find_runtime(app: &tauri::AppHandle) -> Result<Runtime, String> {
    if let Ok(dir) = std::env::var("DSH_DESKTOP_RUNTIME") {
        return bundled_runtime(PathBuf::from(dir));
    }
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let revision_path = repo_root.join("runtime/revision.json");
    if revision_path.is_file() {
        let text = fs::read_to_string(&revision_path).map_err(|e| format!("read {}: {e}", revision_path.display()))?;
        let value: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("parse {}: {e}", revision_path.display()))?;
        let sha = value.get("sha").and_then(|s| s.as_str()).unwrap_or("");
        if !sha.is_empty() {
            let dir = repo_root.join("runtime/build").join(sha);
            if dir.join("dsh/node_modules/@deepseek-ai/dsh/lib/bin.js").is_file() {
                return bundled_runtime(dir);
            }
        }
    }
    if let Some(dir) = release_runtime_dir(app)? {
        return bundled_runtime(dir);
    }
    source_runtime()
}

/// The release bundle's runtime, extracted under the shell-private root:
/// `~/.dsh-desktop/runtime/<sha>/{dsh,tools}` plus an `.ok` marker. Returns
/// Ok(None) in dev builds (no bundled resources); extraction errors surface
/// as Err so a corrupt bundle fails loud instead of silently falling through
/// to a source checkout the user does not have.
///
/// Why tarball resources instead of loose directories: the runtime tree is a
/// pnpm install (3k+ symlinks); tauri-bundler gives no symlink-preservation
/// guarantee, and a dereferencing copy would explode the .pnpm store to GBs.
/// A tar round-trip is link-aware; extraction also lands the tree on a
/// writable volume (App Translocation mounts the .app read-only) and keeps
/// the nested node Mach-O out of the notarization scan later.
fn release_runtime_dir(app: &tauri::AppHandle) -> Result<Option<PathBuf>, String> {
    // Dev builds never consume bundled resources: `tauri dev` resolves
    // resource_dir() to the repo's src-tauri/resources (a leftover from a
    // previous desktop:build), and the ~/.dsh-desktop extraction it feeds
    // can be an older assembly than the repo's runtime/build — dev would
    // silently boot stale code. Release builds have no debug_assertions and
    // take this path as before.
    if cfg!(debug_assertions) {
        return Ok(None);
    }
    let Some(resources) = app.path().resource_dir().ok() else {
        return Ok(None);
    };
    let manifest = resources.join("resources/runtime-revision.json");
    if !manifest.is_file() {
        return Ok(None); // dev build: resources/ carries no bundled runtime
    }
    let text = fs::read_to_string(&manifest).map_err(|e| format!("read {}: {e}", manifest.display()))?;
    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("parse {}: {e}", manifest.display()))?;
    let sha = value.get("sha").and_then(|s| s.as_str()).unwrap_or_default().to_string();
    if sha.is_empty() {
        return Err(format!("bundled runtime-revision.json has no sha: {}", manifest.display()));
    }
    // Content-addressed cache: the .ok marker stores the tarball's sha256,
    // so a same-revision bundle with new content (assembly changes) forces
    // re-extraction instead of booting a stale tree.
    let tarball = value.get("runtimeTarball").and_then(|s| s.as_str()).unwrap_or_default().to_string();
    let root = shell_root()?.join("runtime").join(&sha);
    if !tarball.is_empty()
        && root.join(".ok").is_file()
        && fs::read_to_string(root.join(".ok")).map(|t| t.trim().to_string()).unwrap_or_default() == tarball
    {
        return Ok(Some(root)); // this exact bundle is already extracted
    }
    extract_bundle_tar(
        &resources.join("resources/runtime.tar.gz"),
        &root,
        "dsh/node_modules/@deepseek-ai/dsh/lib/bin.js",
        &tarball,
    )?;
    println!("dsh-desktop: extracted bundled runtime {sha} to {}", root.display());
    Ok(Some(root))
}

/// Extract a bundled tarball into `dir` atomically: extract into a `.tmp`
/// sibling, verify the sentinel entry exists, drop the `.ok` marker, then
/// rename into place (same volume, so the promotion is atomic; an existing
/// `dir` from an older bundle is removed first — last writer wins). A
/// leftover `.tmp` from a crashed boot is removed before we start. The
/// tarballs are generated by our own prepare-desktop-bundle from our own
/// trees (entries never contain absolute paths or `..`), so no
/// path-traversal scrubbing is needed here; the sentinel check catches
/// truncation and corruption anyway.
fn extract_bundle_tar(tar: &Path, dir: &Path, sentinel: &str, ok_content: &str) -> Result<(), String> {
    if !tar.is_file() {
        return Err(format!("bundled tarball missing: {}", tar.display()));
    }
    let parent = dir.parent().ok_or("extraction dir has no parent")?;
    fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    let tmp = dir.with_extension("tmp");
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).map_err(|e| format!("create {}: {e}", tmp.display()))?;
    let mut tar_cmd = Command::new("tar");
    // GNU tar / older bsdtar treat `C:` in `-f C:\...` as a remote host and
    // need `--force-local`. Windows 11 bsdtar 3.8.4 rejects that flag and
    // accepts drive-letter paths as local. Probe `tar --help` once.
    if tar_supports_force_local() {
        tar_cmd.arg("--force-local");
    }
    tar_cmd.arg("-xzf").arg(tar).arg("-C").arg(&tmp);
    hide_console(&mut tar_cmd);
    let status = tar_cmd
        .status()
        .map_err(|e| format!("spawn tar for {}: {e}", tar.display()))?;
    if !status.success() {
        let _ = fs::remove_dir_all(&tmp);
        return Err(format!("tar -xzf {} exited {status}", tar.display()));
    }
    if !tmp.join(sentinel).is_file() {
        let _ = fs::remove_dir_all(&tmp);
        return Err(format!(
            "extracted {} lacks the expected {} — bundled tarball corrupt?",
            tar.display(),
            sentinel
        ));
    }
    fs::write(tmp.join(".ok"), format!("{ok_content}\n"))
        .map_err(|e| format!("write {}: {e}", tmp.join(".ok").display()))?;
    if dir.exists() {
        // Older bundle content at the same path: swap it out.
        fs::remove_dir_all(dir).map_err(|e| format!("remove old {}: {e}", dir.display()))?;
    }
    fs::rename(&tmp, dir).map_err(|e| format!("promote {} to {}: {e}", tmp.display(), dir.display()))?;
    Ok(())
}

fn tar_supports_force_local() -> bool {
    let mut cmd = Command::new("tar");
    cmd.arg("--help");
    hide_console(&mut cmd);
    match cmd.output() {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            stdout.contains("--force-local") || stderr.contains("--force-local")
        }
        Err(_) => false,
    }
}

/// The assembled prebuilt runtime tree (prepare-runtime.mjs output).
fn bundled_runtime(dir: PathBuf) -> Result<Runtime, String> {
    let cli = dir.join("dsh/node_modules/@deepseek-ai/dsh/lib/bin.js");
    if !cli.is_file() {
        return Err(format!("bundled runtime missing CLI entry: {}", cli.display()));
    }
    let Some(node) = bundled_node(&dir) else {
        return Err(format!(
            "bundled runtime missing node binary under {}/tools/node_modules/node",
            dir.display()
        ));
    };
    Ok(Runtime {
        node,
        // Same tsx loader as the source runtime: profiles may carry
        // source-distributed plugins (.ts entries) that plain Node refuses
        // to type-strip under node_modules (ERR_UNSUPPORTED_NODE_MODULES_
        // TYPE_STRIPPING). Resolved from the runtime tree's own tsx dep.
        args_prefix: vec!["--import".to_string(), "tsx/esm".to_string()],
        cli,
        cwd: dir.join("dsh"),
        path_prepend: vec![dir.join("tools/node_modules/.bin"), dir.join("tools/node_modules/node/bin")],
    })
}

/// The `node` npm package lays the binary out differently per OS:
/// Unix `bin/node`, Windows `bin/node.exe` (and a `node.exe` next to
/// `bin/` on some versions).
fn bundled_node(dir: &Path) -> Option<PathBuf> {
    let tools = dir.join("tools/node_modules/node");
    for rel in ["bin/node.exe", "bin/node", "node.exe"] {
        let candidate = tools.join(rel);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// The source checkout (dev): tsx-run CLI from the fork working tree.
fn source_runtime() -> Result<Runtime, String> {
    let checkout = find_checkout()?;
    let node = std::env::var("DSH_NODE").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("node"));
    Ok(Runtime {
        node,
        args_prefix: vec!["--import".to_string(), "tsx/esm".to_string()],
        cli: checkout.join("apps/cli/src/bin.ts"),
        cwd: checkout,
        path_prepend: Vec::new(),
    })
}

/// The bridge package directory: $DSH_DESKTOP_BRIDGE, then the release
/// bundle's extracted resources, then the dev checkout layout.
fn find_bridge(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    if let Ok(from_env) = std::env::var("DSH_DESKTOP_BRIDGE") {
        let p = PathBuf::from(from_env);
        if p.join("package.json").is_file() {
            return Ok(p);
        }
        return Err(format!("DSH_DESKTOP_BRIDGE={} has no package.json", p.display()));
    }
    // Release: extract the bundled bridge tarball (package.json + lib/ +
    // cordis.patch.yml) under the shell-private root, next to the runtime.
    // Same content-hash cache as the runtime: a rebuilt bridge (new lib/)
    // re-extracts instead of booting stale code.
    if let Ok(resources) = app.path().resource_dir() {
        let tar = resources.join("resources/bridge.tar.gz");
        if tar.is_file() {
            let dir = shell_root()?.join("bridge");
            let hash = fs::read_to_string(resources.join("resources/runtime-revision.json"))
                .ok()
                .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
                .and_then(|v| v.get("bridgeTarball").and_then(|s| s.as_str()).map(str::to_string));
            let fresh = hash.as_deref().filter(|h| !h.is_empty())
                .map(|h| dir.join(".ok").is_file() && fs::read_to_string(dir.join(".ok")).map(|t| t.trim() == h).unwrap_or(false))
                .unwrap_or_else(|| dir.join("package.json").is_file()); // no hash in manifest: presence-only
            if !fresh {
                extract_bundle_tar(&tar, &dir, "package.json", hash.as_deref().unwrap_or(""))?;
                println!("dsh-desktop: extracted bundled bridge to {}", dir.display());
            }
            return Ok(dir);
        }
    }
    // Dev builds bake the crate directory.
    let dev = Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugin/dsh-desktop-bridge");
    if dev.join("package.json").is_file() {
        return Ok(dev);
    }
    Err(format!(
        "desktop-bridge package not found at {} (set DSH_DESKTOP_BRIDGE)",
        dev.display()
    ))
}

/// The hierarchical compaction package: an explicit override, the release
/// resource extracted under the shell-private plugin root, or the dev tree.
fn find_compaction_plugin(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    if let Ok(from_env) = std::env::var("DSH_DESKTOP_COMPACTION_PLUGIN") {
        let path = PathBuf::from(from_env);
        if path.join("package.json").is_file() {
            return Ok(path);
        }
        return Err(format!(
            "DSH_DESKTOP_COMPACTION_PLUGIN={} has no package.json",
            path.display()
        ));
    }
    if let Ok(resources) = app.path().resource_dir() {
        let tar = resources.join("resources/compaction-hierarchical.tar.gz");
        if tar.is_file() {
            let dir = shell_root()?.join("plugins").join(COMPACTION_PACKAGE);
            let hash = fs::read_to_string(resources.join("resources/runtime-revision.json"))
                .ok()
                .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
                .and_then(|value| value.get("compactionHierarchicalTarball")
                    .and_then(|item| item.as_str()).map(str::to_string));
            let fresh = hash.as_deref().filter(|value| !value.is_empty())
                .map(|value| dir.join(".ok").is_file()
                    && fs::read_to_string(dir.join(".ok"))
                        .map(|text| text.trim() == value).unwrap_or(false))
                .unwrap_or_else(|| dir.join("package.json").is_file());
            if !fresh {
                extract_bundle_tar(&tar, &dir, "package.json", hash.as_deref().unwrap_or(""))?;
                println!(
                    "dsh-desktop: extracted bundled {} to {}",
                    COMPACTION_PACKAGE,
                    dir.display()
                );
            }
            return Ok(dir);
        }
    }
    let dev = Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugin/dsh-compaction-hierarchical");
    if dev.join("package.json").is_file() {
        return Ok(dev);
    }
    Err(format!(
        "hierarchical compaction package not found at {} (set DSH_DESKTOP_COMPACTION_PLUGIN)",
        dev.display()
    ))
}

/// Point the bridge's value import at the runtime-owned Cordis instance. Dev
/// packages already carry a real dependency tree and are left untouched.
fn ensure_bridge_cordis_link(bridge: &Path, runtime: &Runtime) {
    if let Err(error) = ensure_runtime_package_link(bridge, runtime, "@deepseek-ai/cordis") {
        eprintln!("dsh-desktop: bridge cordis link failed: {error}");
    }
}

/// Release plugin archives intentionally omit node_modules. Link every
/// declared Harness peer to the assembled runtime after `plugin add`, so Node
/// sees the same physical modules as the sidecar. The `.ok` marker limits this
/// mutation to shell-owned extracted packages; source/dev trees keep their own
/// pnpm layout.
fn ensure_bundled_plugin_runtime_links(
    plugin: &Path,
    runtime: &Runtime,
    packages: &[&str],
) -> Result<(), String> {
    if !plugin.join(".ok").is_file() {
        return Ok(());
    }
    for package in packages {
        ensure_runtime_package_link(plugin, runtime, package)?;
    }
    Ok(())
}

/// Idempotently link one plugin dependency to the runtime package realpath.
fn ensure_runtime_package_link(plugin: &Path, runtime: &Runtime, package: &str) -> Result<(), String> {
    let target = resolve_runtime_package(runtime, package).ok_or_else(|| {
        format!("no {package} package under {}", runtime.cwd.display())
    })?;
    let target = fs::canonicalize(&target)
        .map_err(|error| format!("cannot resolve {}: {error}", target.display()))?;
    let link = plugin.join("node_modules").join(package);
    if let Ok(existing) = fs::read_link(&link) {
        let existing_abs = if existing.is_absolute() {
            existing
        } else {
            link.parent().expect("link path always has a parent").join(existing)
        };
        if fs::canonicalize(&existing_abs).is_ok_and(|existing_real| existing_real == target) {
            return Ok(());
        }
        remove_dir_link(&link)
            .map_err(|error| format!("replace {}: {error}", link.display()))?;
    } else if link.exists() {
        return Ok(()); // a real directory in a dev package is not shell-owned
    }
    let parent = link.parent().expect("link path always has a parent");
    fs::create_dir_all(parent).map_err(|error| format!("create {}: {error}", parent.display()))?;
    link_dir(&target, &link)
}

/// Locate a package in a hoisted runtime first, then in an isolated pnpm store.
fn resolve_runtime_package(runtime: &Runtime, package: &str) -> Option<PathBuf> {
    let hoisted = runtime.cwd.join("node_modules").join(package);
    if hoisted.is_dir() {
        return Some(hoisted);
    }
    let encoded = package.replace('/', "+");
    let prefix = format!("{encoded}@");
    let entries = fs::read_dir(runtime.cwd.join("node_modules/.pnpm")).ok()?;
    let mut matches: Vec<PathBuf> = entries
        .flatten()
        .filter(|entry| entry.file_name().to_str().is_some_and(|name| name.starts_with(&prefix)))
        .map(|entry| entry.path().join("node_modules").join(package))
        .filter(|path| path.is_dir())
        .collect();
    matches.sort();
    if matches.len() > 1 {
        eprintln!(
            "dsh-desktop: multiple {package} copies in the runtime ({}), linking the first",
            matches.len()
        );
    }
    matches.into_iter().next()
}

/// Directory symlink (Unix) or directory symlink / junction (Windows).
fn link_dir(target: &Path, link: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)
            .map_err(|e| format!("link {} -> {}: {e}", link.display(), target.display()))
    }
    #[cfg(windows)]
    {
        if std::os::windows::fs::symlink_dir(target, link).is_ok() {
            return Ok(());
        }
        // Junctions do not need Developer Mode / elevation.
        let link_s = link.to_string_lossy().into_owned();
        let target_s = target.to_string_lossy().into_owned();
        let mut command = Command::new("cmd");
        command.args(["/C", "mklink", "/J", &link_s, &target_s]);
        hide_console(&mut command);
        let status = command.status().map_err(|e| format!("mklink /J spawn: {e}"))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("mklink /J {} -> {} exited {status}", link.display(), target.display()))
        }
    }
}

/// Remove a symlink or Windows junction. `remove_file` fails on directory
/// reparse points; try `remove_dir` first on Windows.
fn remove_dir_link(link: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        fs::remove_dir(link).or_else(|_| fs::remove_file(link))
    }
    #[cfg(unix)]
    {
        fs::remove_file(link)
    }
}

/// User home: `%USERPROFILE%` on Windows (native path; Git Bash `$HOME`
/// is `/c/Users/...` and is not a valid Win32 path), `$HOME` elsewhere.
fn user_home() -> Result<PathBuf, String> {
    #[cfg(windows)]
    {
        if let Ok(value) = std::env::var("USERPROFILE") {
            if !value.is_empty() {
                return Ok(PathBuf::from(value));
            }
        }
    }
    if let Ok(value) = std::env::var("HOME") {
        if !value.is_empty() {
            return Ok(PathBuf::from(value));
        }
    }
    Err("$HOME / %USERPROFILE% is not set".into())
}

/// The shell-private root (`~/.dsh-desktop/`): only the shell's own
/// orchestration log (`logs/install.log`) lives here — the sidecar's harness
/// output goes to the shared `$DSH_HOME/logs` (see `sidecar_log_path`).
fn shell_root() -> Result<PathBuf, String> {
    let root = user_home()?.join(".dsh-desktop");
    fs::create_dir_all(root.join("logs")).map_err(|e| format!("create {}: {e}", root.join("logs").display()))?;
    Ok(root)
}

/// The DSH home the sidecar runs with: the user's real `~/.dsh`, shared with
/// the terminal (sessions, workspaces, settings, credentials — the desktop
/// IS another face of the same account). $DSH_HOME overrides for isolation.
/// Caveat: two live harness servers on one home have no locking story —
/// mostly fine for one user (per-session JSONL logs; JSON storages are
/// last-wins whole-file writes), but a shared session being driven from two
/// faces at once is undefined. Coordinated single-instance is an M2 item.
fn dsh_home() -> Result<PathBuf, String> {
    if let Ok(from_env) = std::env::var("DSH_HOME") {
        if !from_env.is_empty() {
            return Ok(PathBuf::from(from_env));
        }
    }
    let home = user_home()?;
    Ok(home.join(".dsh"))
}

/// Idempotently install every desktop-owned package as one profile mutation.
/// pnpm runs only in a sibling shadow DSH_HOME; the real web profile is
/// promoted after a frozen reinstall, package adds, a second frozen reinstall,
/// dependency identity checks, and a full config dump all succeed.
fn run_desktop_plugin_install(
    runtime: &Runtime,
    plugins: &[(&Path, &str)],
    dsh_home: &Path,
    logs: &Path,
    expectation: profile_repair::ProfileExpectation<'_>,
) -> Result<(), String> {
    profile_repair::recover_web_profile(dsh_home)?;
    profile_repair::check_web_profile_expectation(dsh_home, expectation)?;
    let missing = plugins
        .iter()
        .filter(|(plugin, package)| !plugin_already_in_profile(plugin, package, dsh_home))
        .map(|(_, package)| *package)
        .collect::<Vec<_>>();
    if missing.is_empty() {
        println!("dsh-desktop: desktop-owned packages already target this release, skip plugin add");
        return Ok(());
    }
    println!("dsh-desktop: stage web profile update for {}", missing.join(", "));
    let targets = plugins.iter().map(|(plugin, package)| (*package, *plugin)).collect::<Vec<_>>();
    let managed_packages = plugins.iter().map(|(_, package)| *package).collect::<Vec<_>>();
    let preserved_dependencies = resolved_profile_dependencies(dsh_home, &managed_packages)?;
    profile_repair::mutate_web_profile_expected(
        dsh_home,
        &targets,
        expectation,
        |shadow_home, had_original| {
        let shadow_profile = shadow_home.join("profiles/web");
        let protected_files = if had_original {
            capture_protected_profile_files(&shadow_profile)?
        } else {
            Vec::new()
        };
        if had_original && shadow_profile.join("pnpm-lock.yaml").is_file() {
            frozen_profile_install_once(runtime, shadow_home, logs)?;
        }
        for (plugin, package) in plugins {
            if !plugin_already_in_profile(plugin, package, shadow_home) {
                plugin_install_once(runtime, plugin, shadow_home, logs)?;
            }
        }
        // Prove the add result is now lockfile-stable before it can replace
        // the user's real profile.
        frozen_profile_install_once(runtime, shadow_home, logs)?;
        for (plugin, package) in plugins {
            if !plugin_already_in_profile(plugin, package, shadow_home) {
                return Err(format!("staged {package} does not resolve to {}", plugin.display()));
            }
        }
        validate_preserved_dependencies(shadow_home, &preserved_dependencies)?;
        validate_protected_profile_files(&shadow_profile, &protected_files)?;
        validate_profile_config(runtime, shadow_home, logs)
    })
}

/// True when the profile package link already targets this exact package.
fn plugin_already_in_profile(plugin: &Path, package: &str, dsh_home: &Path) -> bool {
    let linked = dsh_home.join("profiles/web/node_modules").join(package);
    match (fs::canonicalize(&linked), fs::canonicalize(plugin)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// One plugin-add attempt against the supplied DSH_HOME.
fn plugin_install_once(runtime: &Runtime, plugin: &Path, dsh_home: &Path, logs: &Path) -> Result<(), String> {
    let log = open_install_log(logs)?;
    let status = cli_command(runtime)
        .arg("plugin")
        .arg("--profile")
        .arg("web")
        .arg("add")
        .arg(plugin)
        .env("DSH_HOME", dsh_home)
        .env("CI", "true")
        .stdout(Stdio::from(log.try_clone().map_err(|e| format!("clone log: {e}"))?))
        .stderr(Stdio::from(log))
        .status()
        .map_err(|e| format!("run plugin add: {e}"))?;
    if !status.success() {
        return Err(format!("plugin --profile web add failed with {status}"));
    }
    Ok(())
}

fn frozen_profile_install_once(runtime: &Runtime, dsh_home: &Path, logs: &Path) -> Result<(), String> {
    let log = open_install_log(logs)?;
    let status = cli_command(runtime)
        .arg("plugin")
        .arg("--profile")
        .arg("web")
        .arg("install")
        .env("DSH_HOME", dsh_home)
        .env("CI", "true")
        .stdout(Stdio::from(log.try_clone().map_err(|e| format!("clone log: {e}"))?))
        .stderr(Stdio::from(log))
        .status()
        .map_err(|e| format!("run frozen profile install: {e}"))?;
    if !status.success() {
        return Err(format!("frozen profile install failed with {status}"));
    }
    Ok(())
}

fn validate_profile_config(runtime: &Runtime, dsh_home: &Path, logs: &Path) -> Result<(), String> {
    let log = open_install_log(logs)?;
    let status = cli_command(runtime)
        .arg("--profile")
        .arg("web")
        .arg("--dump-config")
        .env("DSH_HOME", dsh_home)
        .env("CI", "true")
        .stdout(Stdio::from(log.try_clone().map_err(|e| format!("clone log: {e}"))?))
        .stderr(Stdio::from(log))
        .status()
        .map_err(|e| format!("validate staged profile config: {e}"))?;
    if !status.success() {
        return Err(format!("staged profile config dump failed with {status}"));
    }
    Ok(())
}

fn open_install_log(logs: &Path) -> Result<fs::File, String> {
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(logs.join("logs/install.log"))
        .map_err(|e| format!("open install log: {e}"))
}

fn resolved_profile_dependencies(
    dsh_home: &Path,
    excluded: &[&str],
) -> Result<Vec<(String, PathBuf)>, String> {
    let profile = dsh_home.join("profiles/web");
    let manifest = profile.join("package.json");
    if !manifest.is_file() {
        return Ok(Vec::new());
    }
    let text = fs::read_to_string(&manifest).map_err(|e| format!("read {}: {e}", manifest.display()))?;
    let value: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("parse {}: {e}", manifest.display()))?;
    let mut resolved = Vec::new();
    if let Some(dependencies) = value.get("dependencies").and_then(|value| value.as_object()) {
        for package in dependencies.keys() {
            if excluded.iter().any(|excluded| package == *excluded) {
                continue;
            }
            let linked = profile.join("node_modules").join(package);
            if let Ok(target) = fs::canonicalize(linked) {
                resolved.push((package.clone(), target));
            }
        }
    }
    Ok(resolved)
}

fn validate_preserved_dependencies(
    dsh_home: &Path,
    expected: &[(String, PathBuf)],
) -> Result<(), String> {
    let profile = dsh_home.join("profiles/web/node_modules");
    for (package, target) in expected {
        let actual = fs::canonicalize(profile.join(package))
            .map_err(|e| format!("staged dependency {package} became unresolvable: {e}"))?;
        if &actual != target {
            return Err(format!(
                "staged dependency {package} changed target from {} to {}",
                target.display(),
                actual.display()
            ));
        }
    }
    Ok(())
}

fn capture_protected_profile_files(profile: &Path) -> Result<Vec<(String, Vec<u8>)>, String> {
    ["cordis.patch.yml", "pnpm-workspace.yaml"]
        .into_iter()
        .filter(|name| profile.join(name).is_file())
        .map(|name| {
            fs::read(profile.join(name))
                .map(|bytes| (name.to_string(), bytes))
                .map_err(|e| format!("read protected profile file {name}: {e}"))
        })
        .collect()
}

fn validate_protected_profile_files(
    profile: &Path,
    expected: &[(String, Vec<u8>)],
) -> Result<(), String> {
    for (name, bytes) in expected {
        let actual = fs::read(profile.join(name))
            .map_err(|e| format!("read staged protected profile file {name}: {e}"))?;
        if &actual != bytes {
            return Err(format!("staged profile unexpectedly changed {name}"));
        }
    }
    Ok(())
}

/// Ask the OS for one free loopback port.
fn free_port() -> Result<u16, String> {
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| format!("bind for port pick: {e}"))?;
    let port = listener.local_addr().map_err(|e| format!("read local addr: {e}"))?.port();
    drop(listener);
    Ok(port)
}

/// Resolve this boot's sidecar log file, following the harness `web:log`
/// convention: one `desktop-<yyyymmdd-HHMMSS>.log` per boot under the shared
/// log directory, with a `desktop-latest.log` symlink alongside always naming
/// the newest. `DSH_WEB_LOG_DIR` overrides the directory; the default is
/// `$DSH_HOME/logs` — the same directory terminal `web:log` boots write
/// (`web-*` names), so both faces of the account share one log home.
fn sidecar_log_path(dsh_home: &Path) -> Result<PathBuf, String> {
    let dir = match std::env::var("DSH_WEB_LOG_DIR") {
        Ok(from_env) if !from_env.is_empty() => PathBuf::from(from_env),
        _ => dsh_home.join("logs"),
    };
    fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let log = dir.join(format!("desktop-{stamp}.log"));
    // `ln -sfn`: replace whatever desktop-latest.log pointed at before this
    // boot. Unix-only; Windows gets plain per-boot files for now (M3).
    #[cfg(unix)]
    {
        let latest = dir.join("desktop-latest.log");
        let _ = fs::remove_file(&latest);
        std::os::unix::fs::symlink(&log, &latest).map_err(|e| format!("link {}: {e}", latest.display()))?;
    }
    #[cfg(windows)]
    {
        // Best-effort file symlink; junctions are dirs-only. Failure is
        // fine: per-boot files remain, matching the previous unix-only
        // latest-link gap.
        let latest = dir.join("desktop-latest.log");
        let _ = fs::remove_file(&latest);
        let _ = std::os::windows::fs::symlink_file(&log, &latest);
    }
    Ok(log)
}

/// Spawn the harness web server as a direct node child (no pnpm layer),
/// in its own process group so termination reaches the harness's own
/// children with one atomic signal, returning the per-boot log file its
/// output lands in.
fn spawn_sidecar(runtime: &Runtime, dsh_home: &Path, port: u16) -> Result<PathBuf, String> {
    let log_path = sidecar_log_path(dsh_home)?;
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| format!("open sidecar log: {e}"))?;
    let mut command = sidecar_command(runtime);
    command
        .arg("web")
        .arg("--port")
        .arg(port.to_string())
        // rc.8+ hands the ready URL to the system browser by default; the
        // desktop shell owns its window, so opt out explicitly. Harmless on
        // rc.7-era runtimes (the CLI parser allows unknown options and those
        // never auto-opened).
        .arg("--no-open")
        .env("DSH_HOME", dsh_home)
        .stdout(Stdio::from(log.try_clone().map_err(|e| format!("clone sidecar log: {e}"))?))
        .stderr(Stdio::from(log));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        // Own process group: `kill(-pgid)` later reaches the whole sidecar
        // tree atomically, and terminal Ctrl+C no longer hits the sidecar
        // directly — the shell's handler owns its shutdown ordering.
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        command.creation_flags(CREATE_NO_WINDOW | CREATE_BREAKAWAY_FROM_JOB);
    }
    let child = command.spawn().map_err(|e| format!("spawn sidecar: {e}"))?;
    #[cfg(windows)]
    if let Err(error) = crate::win::assign_sidecar_to_job(&child) {
        eprintln!("dsh-desktop: job-object assign skipped ({error}); falling back to taskkill /T + registry");
    }
    let pid = child.id();
    // Register the sidecar so a later boot can reap it if this shell dies
    // without running its exit path (SIGKILL, crash). Registration is
    // best-effort: without start times the sweep could not tell a recycled
    // pid from the real sidecar, so an unrecordable sidecar simply stays
    // outside the sweep's protection.
    if let (Some(registry), Some(sidecar_lstart), Some(shell_lstart)) =
        (REGISTRY.get(), ps_lstart(pid), ps_lstart(std::process::id()))
    {
        add_registry_entry(
            registry,
            &SidecarEntry {
                sidecar_pid: pid,
                sidecar_lstart,
                shell_pid: std::process::id(),
                shell_lstart,
                port,
                log: log_path.display().to_string(),
            },
        );
    }
    SIDECAR.lock().map_err(|e| e.to_string())?.replace(child);
    Ok(log_path)
}

// ---------------------------------------------------------------------------
// Supervision: the stale-sidecar registry and the reaping sweep.
//
// Why this exists: `tauri dev`'s file watcher restarts the app by calling
// `Child::kill()` on it — SIGKILL on Unix, uncatchable — and never touches
// the app's descendants (its only child-tree kill covers the
// beforeDevCommand process, which this app does not use). So every watcher
// restart leaves the sidecar orphaned: reparented to launchd, still holding
// a random loopback port and the shared `~/.dsh`, with any resumed agents
// alive inside it.
//
// The registry turns "orphaned forever" into "orphaned until the next boot
// reaps it": each shell records the sidecar it spawned, and every boot
// starts by reaping registered sidecars whose shell is provably gone. The
// sweep only ever acts on registered pids — it never scans the process
// table by name — so a terminal's own `dsh web` cannot be collateral
// damage. Pid recycling is guarded by comparing `ps lstart` strings
// recorded at spawn time (a recycled pid shows a new start time).

/// One registered sidecar: enough to identify its process across reboots
/// (pid + start time), its owner (the shell, same guard), and enough
/// context to log a reap usefully (port, log file).
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq)]
struct SidecarEntry {
    sidecar_pid: u32,
    sidecar_lstart: String,
    shell_pid: u32,
    shell_lstart: String,
    port: u16,
    log: String,
}

/// What the sweep does with one registry entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SweepDecision {
    /// Shell and sidecar both live — another running shell owns it.
    Keep,
    /// Sidecar live, shell gone — orphan; terminate it through the ladder.
    Reap,
    /// Sidecar gone (exited, or its pid was recycled) — drop the record.
    Forget,
}

/// The sweep's decision for one entry, given resolved liveness of the
/// shell and the sidecar it points at.
fn sweep_decision(shell_alive: bool, sidecar_alive: bool) -> SweepDecision {
    match (shell_alive, sidecar_alive) {
        (true, true) => SweepDecision::Keep,
        (_, false) => SweepDecision::Forget,
        (false, true) => SweepDecision::Reap,
    }
}

/// The registry file: `~/.dsh-desktop/sidecars.json`.
fn registry_path(shell_root: &Path) -> PathBuf {
    shell_root.join("sidecars.json")
}

/// Load all registered entries; a missing or corrupt registry reads as
/// empty (fail-open: a broken bookkeeping file must not brick the boot,
/// the worst case is one unsupervised sidecar).
fn load_registry(path: &Path) -> Vec<SidecarEntry> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    match serde_json::from_str(&text) {
        Ok(entries) => entries,
        Err(error) => {
            eprintln!("dsh-desktop: sidecar registry {} unreadable ({error}); starting it fresh", path.display());
            Vec::new()
        }
    }
}

/// Atomically replace the registry (write a temp file, rename over).
fn store_registry(path: &Path, entries: &[SidecarEntry]) {
    let tmp = path.with_extension("json.tmp");
    let Ok(json) = serde_json::to_string_pretty(entries) else {
        return;
    };
    if let Err(error) = fs::write(&tmp, json.as_bytes()).and_then(|_| fs::rename(&tmp, path)) {
        eprintln!("dsh-desktop: writing sidecar registry {} failed: {error}", path.display());
    }
}

/// Append (or replace-by-pid) one entry, preserving the other entries
/// as-is. Concurrent writers race last-wins; the loser's entry goes
/// unsupervised — never falsely reaped — which is the safe direction.
fn add_registry_entry(path: &Path, entry: &SidecarEntry) {
    let mut entries = load_registry(path);
    entries.retain(|existing| existing.sidecar_pid != entry.sidecar_pid);
    entries.push(entry.clone());
    store_registry(path, &entries);
}

/// Remove our entry on a graceful exit.
fn unregister_sidecar(pid: u32) {
    let Some(path) = REGISTRY.get().cloned() else {
        return;
    };
    let mut entries = load_registry(&path);
    let before = entries.len();
    entries.retain(|existing| existing.sidecar_pid != pid);
    if entries.len() != before {
        store_registry(&path, &entries);
    }
}

/// A pid's start time from `ps`, or `None` when the pid no longer exists.
/// The string is only ever compared for equality against the value
/// recorded when the entry was written.
#[cfg(unix)]
fn ps_lstart(pid: u32) -> Option<String> {
    let output = std::process::Command::new("ps")
        .arg("-p")
        .arg(pid.to_string())
        .arg("-o")
        .arg("lstart=")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

#[cfg(windows)]
fn ps_lstart(pid: u32) -> Option<String> {
    crate::win::start_token(pid)
}

/// True only when `pid` is alive AND is the same process instance the
/// entry was written for (same start time). A recycled pid reads as dead.
fn pid_matches(pid: u32, recorded_lstart: &str) -> bool {
    ps_lstart(pid).as_deref() == Some(recorded_lstart)
}

/// Terminate a process this shell does not own (an orphaned sidecar):
/// SIGTERM to its whole process group first, SIGKILL after the grace
/// period. Reparented orphans are reaped by launchd, so there is nothing
/// to wait for here.
#[cfg(unix)]
fn term_then_kill(pid: u32) {
    let target = signal_target(pid);
    unsafe { libc::kill(target, libc::SIGTERM) };
    let deadline = Instant::now() + TERM_GRACE;
    while Instant::now() < deadline {
        if ps_lstart(pid).is_none() {
            return;
        }
        std::thread::sleep(LADDER_TICK);
    }
    unsafe { libc::kill(target, libc::SIGKILL) };
}

#[cfg(windows)]
fn term_then_kill(pid: u32) {
    crate::win::taskkill_tree(pid, false);
    let deadline = Instant::now() + TERM_GRACE;
    while Instant::now() < deadline {
        if ps_lstart(pid).is_none() {
            return;
        }
        std::thread::sleep(LADDER_TICK);
    }
    crate::win::taskkill_tree(pid, true);
}

/// Where to aim a sidecar's termination signal: the whole process group
/// when the pid leads one — our spawn puts each sidecar in its own group,
/// so the harness's own children (running tool commands, watchers) are
/// reached by one atomic kernel signal — else the bare pid (registry
/// entries written by pre-process-group builds). Group members that called
/// `setsid` themselves escape; the next-boot sweep remains the last net.
#[cfg(unix)]
fn signal_target(pid: u32) -> libc::pid_t {
    let pid = pid as libc::pid_t;
    let pgid = unsafe { libc::getpgid(pid) };
    if pgid == pid {
        -pid
    } else {
        pid
    }
}

/// Reap registry entries whose shell is gone; keep the rest; persist what
/// survives. Returns the entries it reaped so the caller can log them.
fn sweep_stale_sidecars(path: &Path) -> Vec<SidecarEntry> {
    let entries = load_registry(path);
    if entries.is_empty() {
        return Vec::new();
    }
    let mut kept = Vec::new();
    let mut reaped = Vec::new();
    for entry in entries {
        match sweep_decision(
            pid_matches(entry.shell_pid, &entry.shell_lstart),
            pid_matches(entry.sidecar_pid, &entry.sidecar_lstart),
        ) {
            SweepDecision::Keep => kept.push(entry),
            SweepDecision::Forget => {}
            SweepDecision::Reap => {
                term_then_kill(entry.sidecar_pid);
                reaped.push(entry);
            }
        }
    }
    store_registry(path, &kept);
    reaped
}

/// Handle SIGINT/SIGTERM/SIGHUP the same way as a graceful exit. This
/// covers `kill <app>`, and the case of tauri-cli itself dying without
/// killing the app — SIGKILL (a watcher restart) stays beyond reach and
/// is exactly what the next boot's sweep cleans up. The handler only
/// publishes an atomic; a poller thread does the real shutdown.
#[cfg(unix)]
fn install_terminate_signals() {
    extern "C" fn on_terminate_signal(signal: libc::c_int) {
        SIGNALED.store(signal, std::sync::atomic::Ordering::SeqCst);
    }
    unsafe {
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = on_terminate_signal as *const () as usize;
            action.sa_flags = 0;
            libc::sigaction(signal, &action, std::ptr::null_mut());
        }
    }
    std::thread::spawn(|| loop {
        let signal = SIGNALED.swap(0, std::sync::atomic::Ordering::SeqCst);
        if signal != 0 {
            println!("dsh-desktop: signal {signal} received, shutting the sidecar down");
            kill_sidecar();
            std::process::exit(128 + signal);
        }
        std::thread::sleep(Duration::from_millis(200));
    });
}

/// Poll `GET /` until the webserver answers 2xx, within the budget.
fn wait_ready(port: u16) -> bool {
    let started = Instant::now();
    while started.elapsed() < PROBE_BUDGET {
        if probe_ready(port) {
            return true;
        }
        std::thread::sleep(PROBE_INTERVAL);
    }
    false
}

/// One hand-rolled HTTP probe (no HTTP client dependency for one status line).
fn probe_ready(port: u16) -> bool {
    let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let request = format!("GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut head = [0u8; 32];
    let Ok(n) = stream.read(&mut head) else {
        return false;
    };
    // Any 2xx status means the webserver (and its SPA index) is answering.
    let text = String::from_utf8_lossy(&head[..n]);
    text.starts_with("HTTP/1.1 2") || text.starts_with("HTTP/1.0 2")
}

/// Create the main window on the UI thread with the gate signal injected.
fn open_main_window(app: &tauri::AppHandle, url: &str, e2e: bool) -> Result<(), String> {
    let platform = std::env::consts::OS;
    let gate = format!(
        "window.__DSH_DESKTOP__ = {{ version: 1, shell: 'dsh-desktop', platform: '{platform}' }};"
    );
    let init_script = if e2e {
        format!("{gate}{}", e2e_error_hooks())
    } else {
        gate
    };
    let handle = app.clone();
    let load_url = if e2e { format!("{url}/?e2e=1") } else { url.to_string() };
    let load_parsed: tauri::Url = load_url.parse().map_err(|e| format!("parse {load_url}: {e}"))?;
    app.run_on_main_thread(move || {
        let builder = WebviewWindowBuilder::new(&handle, "main", WebviewUrl::External(load_parsed))
            .title("DeepSeek Harness")
            .inner_size(1400.0, 900.0)
            .initialization_script(&init_script);
        // macOS: the traffic lights float over the page and no native title
        // bar is painted — the bridge plugin lets the columns run edge to
        // edge under the lights (titlebar.ts) and provides the drag region.
        // Other platforms keep the native bar.
        #[cfg(target_os = "macos")]
        let builder = builder.title_bar_style(tauri::TitleBarStyle::Overlay);
        let window = builder.build();
        match window {
            Ok(window) => {
                #[cfg(target_os = "macos")]
                {
                    hide_painted_title(&window);
                    inset_traffic_lights(&window);
                    observe_titlebar_layout(&window);
                }
                println!("dsh-desktop: window built, loading {load_url}");
                if e2e {
                    eval_when_loaded(&handle, &window);
                    watch_e2e_title(handle, window);
                }
            }
            Err(error) => eprintln!("dsh-desktop: window build failed: {error}"),
        }
    })
    .map_err(|e| format!("schedule window creation: {e}"))
}

/// Stop the Overlay titlebar from painting the window title. Tauri's
/// `TitleBarStyle::Overlay` sets `fullSizeContentView` +
/// `titlebarAppearsTransparent` and nothing more — the title string still
/// draws into the floating band, duplicating the page's own branding.
/// `NSWindowTitleVisibility::Hidden` is AppKit's switch for exactly this:
/// it paints no title while keeping the string where the system surfaces
/// (Mission Control, the Window menu) read it from.
///
/// Must run on the main thread (called right after window build inside
/// `run_on_main_thread`).
#[cfg(target_os = "macos")]
fn hide_painted_title(window: &tauri::WebviewWindow) {
    use objc2_app_kit::{NSWindow, NSWindowTitleVisibility};
    match window.ns_window() {
        Ok(raw) => {
            // The pointer is the live NSWindow this WebviewWindow owns.
            let ns_window: &NSWindow = unsafe { &*(raw as *const NSWindow) };
            ns_window.setTitleVisibility(NSWindowTitleVisibility::Hidden);
        }
        Err(error) => eprintln!("dsh-desktop: hiding the painted title failed: {error}"),
    }
}

/// Reposition the traffic lights the way Electron's `WindowButtonsProxy`
/// does: move the **titleBarContainer** (close.superview.superview) so it
/// stays pinned to the window's top edge, then place the three buttons
/// inside it. Only moving the buttons themselves loses to AppKit's layout
/// pass during zoom — the container rides the window height, so the lights
/// do not snap after the animation.
///
/// Targets match the bridge band: circle center y19 (`top:8` / height 22),
/// close circle left edge x16 (sidebar content line).
///
/// Must run on the main thread.
#[cfg(target_os = "macos")]
fn inset_traffic_lights(window: &tauri::WebviewWindow) {
    use objc2_app_kit::{NSWindow, NSWindowButton};
    use objc2_foundation::NSPoint;
    const BUTTON_SIZE: f64 = 14.0;
    const MARGIN_X: f64 = 15.0;
    const CENTER_Y_FROM_TOP: f64 = 19.0;
    const CONTAINER_HEIGHT: f64 = 28.0;
    match window.ns_window() {
        Ok(raw) => {
            let ns_window: &NSWindow = unsafe { &*(raw as *const NSWindow) };
            let Some(close) = ns_window.standardWindowButton(NSWindowButton::CloseButton) else {
                return;
            };
            let Some(mini) = ns_window.standardWindowButton(NSWindowButton::MiniaturizeButton) else {
                return;
            };
            let Some(zoom) = ns_window.standardWindowButton(NSWindowButton::ZoomButton) else {
                return;
            };
            let Some(title_bar) = (unsafe { close.superview().and_then(|view| view.superview()) }) else {
                return;
            };
            let window_height = ns_window.frame().size.height;
            let spacing = mini.frame().origin.x - close.frame().origin.x;
            let spacing = if spacing > 0.0 { spacing } else { 23.0 };
            let mut container = title_bar.frame();
            container.size.height = CONTAINER_HEIGHT;
            container.origin.y = window_height - CONTAINER_HEIGHT;
            title_bar.setFrame(container);
            let button_y = CONTAINER_HEIGHT - CENTER_Y_FROM_TOP - BUTTON_SIZE / 2.0;
            close.setFrameOrigin(NSPoint::new(MARGIN_X, button_y));
            mini.setFrameOrigin(NSPoint::new(MARGIN_X + spacing, button_y));
            zoom.setFrameOrigin(NSPoint::new(MARGIN_X + spacing * 2.0, button_y));
        }
        Err(error) => eprintln!("dsh-desktop: insetting the traffic lights failed: {error}"),
    }
}

/// Hide the titleBarContainer for the duration of a fullscreen transition
/// so the lights do not jump (Electron `WindowButtonsProxy setVisible:NO`
/// on will-leave). Shown again by `inset_traffic_lights` after the
/// transition.
#[cfg(target_os = "macos")]
fn set_titlebar_container_hidden(window: &tauri::WebviewWindow, hidden: bool) {
    use objc2_app_kit::{NSWindow, NSWindowButton};
    let Ok(raw) = window.ns_window() else { return };
    let ns_window: &NSWindow = unsafe { &*(raw as *const NSWindow) };
    let Some(close) = ns_window.standardWindowButton(NSWindowButton::CloseButton) else {
        return;
    };
    if let Some(title_bar) = unsafe { close.superview().and_then(|view| view.superview()) } {
        title_bar.setHidden(hidden);
    }
}

/// Subscribe to AppKit window notifications that fire **during** zoom /
/// fullscreen, not just when Tauri emits `Resized` at the end. Each callback
/// re-runs `inset_traffic_lights` on the main thread. Tokens are leaked for
/// the process lifetime (one main window).
#[cfg(target_os = "macos")]
fn observe_titlebar_layout(window: &tauri::WebviewWindow) {
    use block2::RcBlock;
    use objc2::runtime::AnyObject;
    use objc2_app_kit::{
        NSWindowDidEndLiveResizeNotification, NSWindowDidEnterFullScreenNotification,
        NSWindowDidExitFullScreenNotification, NSWindowDidResizeNotification,
        NSWindowWillEnterFullScreenNotification, NSWindowWillExitFullScreenNotification,
    };
    use objc2_foundation::{NSNotification, NSNotificationCenter};
    use std::ptr::NonNull;

    let Ok(raw) = window.ns_window() else {
        eprintln!("dsh-desktop: titlebar layout observer: no ns_window");
        return;
    };
    let ns_window_obj = raw as *const AnyObject;
    let center = NSNotificationCenter::defaultCenter();

    let redraw = {
        let window = window.clone();
        RcBlock::new(move |_: NonNull<NSNotification>| {
            set_titlebar_container_hidden(&window, false);
            inset_traffic_lights(&window);
        })
    };
    let hide = {
        let window = window.clone();
        RcBlock::new(move |_: NonNull<NSNotification>| {
            set_titlebar_container_hidden(&window, true);
        })
    };

    let object = unsafe { ns_window_obj.as_ref() };
    let redraw_names = unsafe {
        [
            NSWindowDidResizeNotification,
            NSWindowDidEndLiveResizeNotification,
            NSWindowDidEnterFullScreenNotification,
            NSWindowDidExitFullScreenNotification,
        ]
    };
    for name in redraw_names {
        let observer = unsafe {
            center.addObserverForName_object_queue_usingBlock(Some(name), object, None, &redraw)
        };
        std::mem::forget(observer);
    }
    let hide_names = unsafe {
        [NSWindowWillEnterFullScreenNotification, NSWindowWillExitFullScreenNotification]
    };
    for name in hide_names {
        let observer = unsafe {
            center.addObserverForName_object_queue_usingBlock(Some(name), object, None, &hide)
        };
        std::mem::forget(observer);
    }
    std::mem::forget(redraw);
    std::mem::forget(hide);
}

/// Inject the e2e probe by eval once the SPA has had time to settle: init
/// scripts run before page scripts exist, while the probe wants the loaded
/// DOM. Waiting on the URL becoming the final http document is the cheapest
/// settle signal; the probe's own waitFs cover the rest.
fn eval_when_loaded(app: &tauri::AppHandle, window: &tauri::WebviewWindow) {
    let app = app.clone();
    let window = window.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(5));
        // wry's WKWebView eval must run on the main thread; scheduling through
        // the app handle keeps the background thread out of the webview.
        if let Err(error) = app.run_on_main_thread(move || {
            if let Err(error) = window.eval(&e2e_probe_script()) {
                println!("dsh-desktop e2e: probe eval failed: {error}");
            }
        }) {
            println!("dsh-desktop e2e: probe scheduling failed: {error}");
        }
    });
}

/// Page-error hooks for the e2e run: route window errors and unhandled
/// rejections into the location-hash channel the shell polls.
fn e2e_error_hooks() -> String {
    r#"
(function () {
  var send = function (kind, text) {
    try {
      if ((location.hash || '').indexOf('#dsh-e2e-') === 0) return
      history.replaceState(null, '', '#dsh-' + kind + '-' + encodeURIComponent(String(text).slice(0, 200)))
    } catch (error) { /* nothing else to do */ }
  }
  window.addEventListener('error', function (event) {
    send('err', (event.error && event.error.message) || event.message || 'error')
  })
  window.addEventListener('unhandledrejection', function (event) {
    send('rej', (event.reason && event.reason.message) || String(event.reason) || 'rejection')
  })
})()
"#
    .to_string()
}

/// The e2e probe: gate → IPC carrier → badge DOM → save-file roundtrip.
/// Every stage rejects through the promise chain, so any failure lands in
/// the window title the shell polls.
fn e2e_probe_script() -> String {
    r#"
(function () {
  function report(verdict) {
    try { history.replaceState(null, '', '#dsh-e2e-' + encodeURIComponent(verdict)) } catch (error) { /* hash channel dead */ }
    try {
      var reported = window.__TAURI_INTERNALS__.invoke('dsh_desktop_e2e_report', { verdict: verdict })
      if (reported && typeof reported.catch === 'function') reported.catch(function () { /* IPC refused; the hash above already carries the verdict */ })
    } catch (error) { /* IPC carrier unusable */ }
  }
  function waitFor(pred, timeoutMs, what) {
    return new Promise(function (resolve, reject) {
      var started = Date.now()
      var tick = function () {
        var value
        try { value = pred() } catch (error) { return reject(error) }
        if (value) return resolve(undefined)
        if (Date.now() - started > timeoutMs) return reject(new Error('timeout waiting for ' + what))
        setTimeout(tick, 500)
      }
      setTimeout(tick, 500)
    })
  }
  var stage = function (name) {
    try { history.replaceState(null, '', '#dsh-stage-' + name) } catch (error) { /* best effort */ }
  }
  Promise.resolve()
    .then(function () {
      stage('gate')
      if (window.__DSH_DESKTOP__ === undefined) throw new Error('gate signal missing')
      return waitFor(function () { return window.__TAURI_INTERNALS__ !== undefined }, 30000, '__TAURI_INTERNALS__')
    })
    .then(function () {
      stage('app-root')
      return waitFor(function () {
        var root = document.getElementById('root')
        return root !== null && root.childElementCount > 0
      }, 60000, 'app root content (boot graph: ' + (window.__DSH_BOOT__ ? 'present' : 'absent') + ')')
    })
    .then(function () {
      stage('badge')
      return waitFor(function () { return document.querySelector('[data-desktop-badge]') !== null }, 60000, 'badge DOM')
    })
    .then(function () {
      stage('save-invoke')
      return Promise.race([
        window.__TAURI_INTERNALS__.invoke('dsh_desktop_save_file', { name: 'dsh-e2e-probe.txt', base64: btoa('dsh-desktop e2e check') }),
        new Promise(function (_, reject) { setTimeout(function () { reject(new Error('save invoke timed out')) }, 30000) }),
      ])
    })
    .then(function (saved) {
      if (typeof saved !== 'string' || saved.length === 0) throw new Error('save returned no path')
      report('OK')
    })
    .catch(function (error) {
      var diag = 'root=' + (document.getElementById('root') ? document.getElementById('root').childElementCount : -1)
        + ',overlay=' + (document.querySelector('div[data-slot="shell.overlay"]') ? document.querySelector('div[data-slot="shell.overlay"]').childElementCount : -1)
      var match = document.body.innerText.match(/\/plugins\/[^\s]+client\.js\?rev=[a-f0-9]+/)
      if (match === null) {
        report('FAIL:' + String(error && error.message ? error.message : error).slice(0, 120) + ' | ' + diag + ' | body=' + document.body.innerText.replace(/\s+/g, ' ').slice(0, 300))
        return
      }
      fetch(match[0]).then(function (r) {
        if (!r.ok) throw new Error('retry fetch status ' + r.status)
        return r.text()
      }).then(function (text) {
        report('FAIL:retry-ok bytes=' + text.length + ' | ' + diag)
      }).catch(function (fetchError) {
        report('FAIL:retry-' + String(fetchError && fetchError.message ? fetchError.message : fetchError).slice(0, 120) + ' | ' + diag)
      })
    })
})()
"#
    .to_string()
}

/// Poll both verdict channels for the e2e result; log it and optionally exit.
/// The title channel is dead on macOS (WKWebView document.title does not
/// sync to the NSWindow title), so the channels are the IPC command and the
/// location-hash fallback written by the probe.
fn watch_e2e_title(app: tauri::AppHandle, window: tauri::WebviewWindow) {
    std::thread::spawn(move || {
        let started = Instant::now();
        let exit_when_done = std::env::var("DSH_DESKTOP_E2E_EXIT").ok().as_deref() == Some("1");
        let mut last_fragment = String::new();
        while started.elapsed() < Duration::from_secs(120) {
            std::thread::sleep(Duration::from_millis(500));
            // Hash diagnostics: read the URL only after navigation must have
            // committed, and contain wry's nil-URL panic for good measure.
            if started.elapsed() >= Duration::from_secs(20) {
                let read = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| window.url().ok()));
                if let Ok(Some(url)) = read {
                    let fragment = url.fragment().unwrap_or_default();
                    if fragment != last_fragment && !fragment.is_empty() {
                        println!("dsh-desktop e2e: fragment -> {fragment}");
                        last_fragment = fragment.to_string();
                    }
                }
            }
            let Some(verdict) = ipc_verdict() else { continue };
            println!("dsh-desktop e2e: DSH_E2E_{verdict}");
            use std::io::Write as _;
            let _ = std::io::stdout().flush();
            if exit_when_done {
                app.exit(if verdict.starts_with("OK") { 0 } else { 2 });
            }
            return;
        }
        println!("dsh-desktop e2e: timed out waiting for the probe verdict");
        if exit_when_done {
            app.exit(3);
        }
    });
}

/// The verdict stored by the IPC report command, if any.
fn ipc_verdict() -> Option<String> {
    E2E_VERDICT.lock().ok().and_then(|guard| guard.clone())
}

/// IPC: the e2e probe's verdict report (primary verdict channel).
#[tauri::command]
fn dsh_desktop_e2e_report(verdict: String) -> Result<(), String> {
    if let Ok(mut guard) = E2E_VERDICT.lock() {
        *guard = Some(verdict);
    }
    Ok(())
}

/// Replace the shared updater snapshot. Callback sites deliberately ignore a
/// poisoned mutex: a status rendering failure must not abort a verified update.
fn set_update_status(status: DesktopUpdateStatus) {
    if let Ok(mut current) = UPDATE_STATUS.lock() {
        *current = status;
    }
}

/// Clone the current updater snapshot for IPC and failure recovery.
fn update_status_snapshot() -> Result<DesktopUpdateStatus, String> {
    UPDATE_STATUS
        .lock()
        .map(|status| status.clone())
        .map_err(|_| "update status unavailable".to_string())
}

/// Claim the updater for a check operation while retaining a known target for
/// failure recovery.
fn claim_update_check(status: &mut DesktopUpdateStatus) -> Result<Option<String>, String> {
    if status.is_busy() {
        return Err("update operation already in progress".to_string());
    }
    if matches!(status, DesktopUpdateStatus::Ready { .. }) {
        return Err("downloaded update awaiting confirmation".to_string());
    }
    let version = status.version();
    *status = DesktopUpdateStatus::Checking;
    Ok(version)
}

fn begin_update_check() -> Result<Option<String>, String> {
    let mut status = UPDATE_STATUS
        .lock()
        .map_err(|_| "update status unavailable".to_string())?;
    claim_update_check(&mut status)
}

/// Download is allowed only after a successful check exposed a target.
fn claim_update_download(status: &mut DesktopUpdateStatus) -> Result<String, String> {
    if status.is_busy() {
        return Err("update operation already in progress".to_string());
    }
    let version = match &*status {
        DesktopUpdateStatus::Available { version, .. } => version.clone(),
        _ => return Err("no checked update available".to_string()),
    };
    *status = DesktopUpdateStatus::Preparing {
        version: Some(version.clone()),
    };
    Ok(version)
}

fn begin_update_download() -> Result<String, String> {
    let mut status = UPDATE_STATUS
        .lock()
        .map_err(|_| "update status unavailable".to_string())?;
    claim_update_download(&mut status)
}

/// Installation is a separate explicit action after signature verification.
fn claim_update_install(status: &mut DesktopUpdateStatus) -> Result<String, String> {
    if status.is_busy() {
        return Err("update operation already in progress".to_string());
    }
    let version = match &*status {
        DesktopUpdateStatus::Ready { version } => version.clone(),
        _ => return Err("no downloaded update ready".to_string()),
    };
    *status = DesktopUpdateStatus::Installing {
        version: version.clone(),
    };
    Ok(version)
}

fn begin_update_install() -> Result<String, String> {
    let mut status = UPDATE_STATUS
        .lock()
        .map_err(|_| "update status unavailable".to_string())?;
    claim_update_install(&mut status)
}

/// Add one updater download chunk without allowing integer wraparound.
fn add_update_chunk(status: &mut DesktopUpdateStatus, chunk: usize, content_length: Option<u64>) {
    let DesktopUpdateStatus::Downloading {
        downloaded, total, ..
    } = status
    else {
        return;
    };
    *downloaded = downloaded.saturating_add(chunk as u64);
    if total.is_none() {
        *total = content_length;
    }
}

/// IPC: check the updater endpoint for a newer release. The same result also
/// advances the process-wide snapshot consumed by both browser surfaces.
#[tauri::command]
async fn dsh_desktop_check_update(app: tauri::AppHandle) -> Result<serde_json::Value, String> {
    use tauri_plugin_updater::UpdaterExt as _;

    let expected_version = begin_update_check()?;
    let result = async {
        let updater = app
            .updater()
            .map_err(|e| format!("updater unavailable: {e}"))?;
        updater.check().await.map_err(|e| format!("check failed: {e}"))
    }
    .await;

    match result {
        Ok(Some(update)) => {
            let version = update.version;
            let notes = update.body.unwrap_or_default();
            set_update_status(DesktopUpdateStatus::Available {
                version: version.clone(),
                notes: notes.clone(),
            });
            Ok(serde_json::json!({ "update": { "version": version, "notes": notes } }))
        }
        Ok(None) => {
            set_update_status(DesktopUpdateStatus::Current);
            Ok(serde_json::json!({ "update": null }))
        }
        Err(message) => {
            set_update_status(DesktopUpdateStatus::Failed {
                version: expected_version,
                message: message.clone(),
            });
            Err(message)
        }
    }
}

/// IPC: read the latest updater snapshot. The browser polls only while an
/// update is visible or active, keeping the idle desktop free of IPC traffic.
#[tauri::command]
fn dsh_desktop_update_status() -> Result<DesktopUpdateStatus, String> {
    update_status_snapshot()
}

/// IPC: single-flight recheck plus streaming download and signature
/// verification. The verified bytes remain process-local until confirmation.
#[tauri::command]
async fn dsh_desktop_download_update(app: tauri::AppHandle) -> Result<(), String> {
    use tauri_plugin_updater::UpdaterExt as _;

    let expected_version = begin_update_download()?;
    let result = async {
        {
            let mut downloaded = DOWNLOADED_UPDATE
                .lock()
                .map_err(|_| "downloaded update storage unavailable".to_string())?;
            *downloaded = None;
        }
        let updater = app
            .updater()
            .map_err(|e| format!("updater unavailable: {e}"))?;
        let update = updater
            .check()
            .await
            .map_err(|e| format!("check failed: {e}"))?
            .ok_or_else(|| "no update available".to_string())?;
        let version = update.version.clone();
        set_update_status(DesktopUpdateStatus::Downloading {
            version: version.clone(),
            downloaded: 0,
            total: None,
        });
        let bytes = update
            .download(
                move |chunk, content_length| {
                    if let Ok(mut status) = UPDATE_STATUS.lock() {
                        add_update_chunk(&mut status, chunk, content_length);
                    }
                },
                || {},
            )
            .await
            .map_err(|e| format!("download failed: {e}"))?;
        {
            let mut downloaded = DOWNLOADED_UPDATE
                .lock()
                .map_err(|_| "downloaded update storage unavailable".to_string())?;
            *downloaded = Some(DownloadedUpdate { update, bytes });
        }
        set_update_status(DesktopUpdateStatus::Ready { version });
        Ok::<(), String>(())
    }
    .await;

    if let Err(message) = result {
        let version = update_status_snapshot()
            .ok()
            .and_then(|status| status.version())
            .or(Some(expected_version));
        set_update_status(DesktopUpdateStatus::Failed {
            version,
            message: message.clone(),
        });
        return Err(message);
    }
    Ok(())
}

/// IPC: install the already verified package after explicit confirmation, then
/// replace this process. Successful calls do not resolve in the browser.
#[tauri::command]
async fn dsh_desktop_install_update(app: tauri::AppHandle) -> Result<(), String> {
    let expected_version = begin_update_install()?;
    let downloaded = DOWNLOADED_UPDATE
        .lock()
        .map_err(|_| "downloaded update storage unavailable".to_string())
        .and_then(|mut downloaded| {
            downloaded
                .take()
                .ok_or_else(|| "downloaded update missing".to_string())
        });
    let downloaded = match downloaded {
        Ok(downloaded) => downloaded,
        Err(message) => {
            set_update_status(DesktopUpdateStatus::Failed {
                version: Some(expected_version),
                message: message.clone(),
            });
            return Err(message);
        }
    };
    if downloaded.update.version != expected_version {
        let message = "downloaded update version changed".to_string();
        set_update_status(DesktopUpdateStatus::Failed {
            version: Some(expected_version),
            message: message.clone(),
        });
        return Err(message);
    }
    if let Err(error) = downloaded.update.install(&downloaded.bytes) {
        let message = format!("install failed: {error}");
        set_update_status(DesktopUpdateStatus::Failed {
            version: Some(expected_version),
            message: message.clone(),
        });
        return Err(message);
    }
    set_update_status(DesktopUpdateStatus::Restarting {
        version: expected_version,
    });

    // Never returns: the process is replaced by the new version.
    #[allow(unreachable_code)]
    {
        tauri::process::restart(&app.env());
        Ok(())
    }
}

/// IPC: open a URL in the system browser (scheme-whitelisted).
#[tauri::command]
fn dsh_desktop_open_external(url: String) -> Result<(), String> {
    let lower = url.to_ascii_lowercase();
    let allowed = lower.starts_with("http://") || lower.starts_with("https://")
        || lower.starts_with("mailto:") || lower.starts_with("tel:");
    if !allowed {
        return Err(format!("scheme not allowed: {url}"));
    }
    let opener = match std::env::consts::OS {
        "macos" => ("open", vec![url]),
        // Empty title arg so `start` does not treat a quoted URL as the window title.
        "windows" => ("cmd", vec!["/C".to_string(), "start".to_string(), String::new(), url]),
        _ => ("xdg-open", vec![url]),
    };
    let mut command = Command::new(opener.0);
    command.args(opener.1);
    hide_console(&mut command);
    command
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("open external: {e}"))
}

/// IPC: fire a native system notification (best effort).
#[tauri::command]
fn dsh_desktop_notify(title: String, body: String) -> Result<(), String> {
    let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
    let status = match std::env::consts::OS {
        "macos" => Command::new("osascript")
            .arg("-e")
            .arg(format!("display notification \"{}\" with title \"{}\"", esc(&body), esc(&title)))
            .status(),
        "windows" => windows_toast(&title, &body),
        _ => Command::new("notify-send")
            .arg(title)
            .arg(body)
            .status(),
    };
    match status {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(format!("notify exited {status}")),
        Err(e) => Err(format!("notify spawn: {e}")),
    }
}

#[cfg(windows)]
fn windows_toast(title: &str, body: &str) -> std::io::Result<std::process::ExitStatus> {
    let xml_esc = |s: &str| {
        s.replace(['\r', '\n'], " ")
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    };
    let xml = format!(
        "<toast><visual><binding template=\"ToastGeneric\"><text>{}</text><text>{}</text></binding></visual></toast>",
        xml_esc(title),
        xml_esc(body)
    );
    let script = format!(
        "[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] | Out-Null\n\
         [Windows.Data.Xml.Dom.XmlDocument, Windows.Data.Xml.Dom.XmlDocument, ContentType = WindowsRuntime] | Out-Null\n\
         $xml = New-Object Windows.Data.Xml.Dom.XmlDocument\n\
         $xml.LoadXml(@'\n{xml}\n'@)\n\
         $toast = [Windows.UI.Notifications.ToastNotification]::new($xml)\n\
         [Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('dev.dsh.desktop').Show($toast)\n"
    );
    let encoded = utf16_le_base64(&script);
    let mut command = Command::new("powershell");
    command.args(["-NoProfile", "-NonInteractive", "-EncodedCommand", &encoded]);
    hide_console(&mut command);
    command.status()
}

#[cfg(windows)]
fn utf16_le_base64(script: &str) -> String {
    let bytes: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes)
}

#[cfg(not(windows))]
fn windows_toast(_title: &str, _body: &str) -> std::io::Result<std::process::ExitStatus> {
    unreachable!("windows_toast is only called on Windows")
}

/// IPC: write base64 bytes into the user's Downloads directory.
#[tauri::command]
fn dsh_desktop_save_file(name: String, base64: String) -> Result<String, String> {
    let sanitized = name
        .rsplit(['/', '\\'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("download")
        .to_string();
    let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, base64.as_bytes())
        .map_err(|e| format!("base64 decode: {e}"))?;
    let dir = downloads_dir()?;
    let path = unique_path(&dir, &sanitized);
    fs::write(&path, bytes).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(path.display().to_string())
}

/// The user's Downloads directory ($HOME/Downloads, created on demand).
fn downloads_dir() -> Result<PathBuf, String> {
    let home = user_home()?;
    let dir = home.join("Downloads");
    fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    Ok(dir)
}

/// `name.ext` → first free `name.ext` / `name-1.ext` / `name-2.ext` / …
fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) => (stem.to_string(), format!(".{ext}")),
        None => (name.to_string(), String::new()),
    };
    for n in 1.. {
        let candidate = dir.join(format!("{stem}-{n}{ext}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!("the loop returns on the first free candidate")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(sidecar_pid: u32, shell_pid: u32) -> SidecarEntry {
        SidecarEntry {
            sidecar_pid,
            sidecar_lstart: "sidecar-lstart".into(),
            shell_pid,
            shell_lstart: "shell-lstart".into(),
            port: 39000,
            log: "/tmp/desktop-test.log".into(),
        }
    }

    fn scratch_registry(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("dsh-desktop-test-{}-{name}.json", std::process::id()));
        let _ = fs::remove_file(&path);
        path
    }

    fn scratch_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("dsh-desktop-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn configured_path(command: &Command) -> Vec<PathBuf> {
        let value = command
            .get_envs()
            .find(|(name, _)| *name == OsStr::new("PATH"))
            .and_then(|(_, value)| value)
            .expect("command configures PATH");
        std::env::split_paths(value).collect()
    }

    #[test]
    fn adoption_plan_prompts_only_for_unowned_existing_or_restored_homes() {
        use profile_adoption::AdoptionStatus;

        assert_eq!(plan_profile_adoption(false, None), AdoptionPlan::StartFresh);
        assert_eq!(plan_profile_adoption(true, None), AdoptionPlan::AskExisting);
        assert_eq!(
            plan_profile_adoption(true, Some(AdoptionStatus::Adopting)),
            AdoptionPlan::Resume
        );
        assert_eq!(
            plan_profile_adoption(true, Some(AdoptionStatus::Active)),
            AdoptionPlan::Resume
        );
        assert_eq!(
            plan_profile_adoption(true, Some(AdoptionStatus::RestorePending)),
            AdoptionPlan::Resume
        );
        for status in [
            AdoptionStatus::ConsentRequired,
            AdoptionStatus::Restored,
            AdoptionStatus::RestoreAbandoned,
        ] {
            assert_eq!(
                plan_profile_adoption(false, Some(status)),
                AdoptionPlan::AskExisting
            );
        }
    }

    #[test]
    fn corrupt_active_backup_can_be_abandoned_without_deleting_its_files() {
        let root = scratch_dir("corrupt-adoption-backup");
        let shell = root.join("shell");
        let home = root.join("home");
        let profile = home.join("profiles/web");
        fs::create_dir_all(profile.join("node_modules")).unwrap();
        fs::write(profile.join("package.json"), "{}\n").unwrap();
        fs::write(profile.join("pnpm-lock.yaml"), "lock\n").unwrap();
        fs::write(profile.join("pnpm-workspace.yaml"), "packages: []\n").unwrap();
        fs::write(profile.join("cordis.patch.yml"), "[]\n").unwrap();
        let summary = profile_adoption::inspect_home(&home).unwrap();
        let backup =
            profile_adoption::create_backup(&shell, &summary.canonical_home).unwrap();
        let adopting = profile_adoption::start_record(
            &shell,
            &summary.canonical_home,
            profile_adoption::AdoptionOrigin::ExistingHome,
            true,
            Some(backup.clone()),
        )
        .unwrap();
        profile_adoption::transition(
            &shell,
            &adopting,
            profile_adoption::AdoptionStatus::Active,
            Some(backup.clone()),
        )
        .unwrap();
        fs::write(backup.profile.join("package.json"), "tampered\n").unwrap();

        let result = native_dialog::with_test_choice(native_dialog::Choice::Primary, || {
            prepare_profile_adoption(&shell, &summary).unwrap()
        });
        assert!(result.is_none());
        let latest = profile_adoption::latest_record(&shell, &summary.canonical_home)
            .unwrap()
            .unwrap();
        assert_eq!(
            latest.status,
            profile_adoption::AdoptionStatus::ConsentRequired
        );
        assert!(latest.backup.is_none());
        assert!(backup.root.is_dir());
        let replacement =
            profile_adoption::create_backup(&shell, &summary.canonical_home).unwrap();
        profile_adoption::prune_other_backups(
            &shell,
            &summary.canonical_home,
            &replacement,
        )
        .unwrap();
        assert!(backup.root.is_dir());
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn restore_pending_rebuilds_and_promotes_the_saved_snapshot() {
        use std::os::unix::fs::PermissionsExt;

        let root = scratch_dir("restore-pending");
        let shell = root.join("shell");
        let home = root.join("home");
        let profile = home.join("profiles/web");
        fs::create_dir_all(profile.join("node_modules")).unwrap();
        fs::create_dir_all(shell.join("logs")).unwrap();
        fs::create_dir_all(home.join("sessions")).unwrap();
        fs::write(profile.join("package.json"), "{\"name\":\"before\"}\n").unwrap();
        fs::write(profile.join("pnpm-lock.yaml"), "lockfileVersion: 9\n").unwrap();
        fs::write(profile.join("pnpm-workspace.yaml"), "packages:\n  - .\n").unwrap();
        fs::write(profile.join("cordis.patch.yml"), "[]\n").unwrap();
        fs::write(home.join("sessions/one.jsonl"), "session\n").unwrap();

        let canonical = fs::canonicalize(&home).unwrap();
        let backup = profile_adoption::create_backup(&shell, &canonical).unwrap();
        let adopting = profile_adoption::start_record(
            &shell,
            &canonical,
            profile_adoption::AdoptionOrigin::ExistingHome,
            true,
            Some(backup.clone()),
        )
        .unwrap();
        fs::remove_dir_all(&profile).unwrap();
        let source_identity = current_restore_source(&canonical).unwrap();
        assert_eq!(source_identity, MISSING_RESTORE_SOURCE);
        let pending = profile_adoption::begin_restore(&shell, &adopting, source_identity).unwrap();

        let cli = root.join("fake-dsh.sh");
        fs::write(
            &cli,
            "#!/bin/sh\nset -eu\ncase \" $* \" in\n  *\" --dump-config \"*) printf '[]\\n' ;;\n  *\" install \"*) mkdir -p \"$DSH_HOME/profiles/web/node_modules\" ;;\n  *) exit 2 ;;\nesac\n",
        )
        .unwrap();
        fs::set_permissions(&cli, fs::Permissions::from_mode(0o700)).unwrap();
        let runtime = Runtime {
            node: PathBuf::from("/bin/sh"),
            args_prefix: Vec::new(),
            cli,
            cwd: root.clone(),
            path_prepend: Vec::new(),
        };

        let restored =
            restore_adoption_backup(&runtime, &home, &shell, &canonical, &pending).unwrap();
        assert_eq!(restored.status, profile_adoption::AdoptionStatus::Restored);
        assert!(profile_adoption::current_profile_matches_backup(&canonical, &backup).unwrap());
        assert_eq!(
            fs::read_to_string(home.join("sessions/one.jsonl")).unwrap(),
            "session\n"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn process_path_prefers_and_deduplicates_structured_directories() {
        let root = scratch_dir("process-path");
        let runtime = root.join("runtime bin");
        let inherited = root.join("inherited");
        fs::create_dir_all(&runtime).unwrap();
        fs::create_dir_all(&inherited).unwrap();
        let inherited_value = std::env::join_paths([runtime.clone(), inherited.clone()]).unwrap();
        let value = compose_process_path(
            std::slice::from_ref(&runtime),
            Some(inherited_value.as_os_str()),
        )
        .unwrap();
        let paths = std::env::split_paths(&value).collect::<Vec<_>>();
        assert_eq!(paths, vec![runtime, inherited]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn host_cli_directories_apply_only_to_the_sidecar() {
        let root = scratch_dir("sidecar-path");
        let runtime_bin = root.join("runtime-bin");
        let host_bin = root.join("host-bin");
        fs::create_dir_all(&runtime_bin).unwrap();
        fs::create_dir_all(&host_bin).unwrap();
        let runtime = Runtime {
            node: PathBuf::from("node"),
            args_prefix: Vec::new(),
            cli: PathBuf::from("dsh"),
            cwd: root.clone(),
            path_prepend: vec![runtime_bin.clone()],
        };

        let profile_paths = configured_path(&cli_command(&runtime));
        assert_eq!(profile_paths.first(), Some(&runtime_bin));
        assert!(!profile_paths.contains(&host_bin));

        let sidecar_paths = configured_path(&sidecar_command_with_host_dirs(
            &runtime,
            std::slice::from_ref(&host_bin),
        ));
        assert_eq!(sidecar_paths.first(), Some(&runtime_bin));
        assert_eq!(sidecar_paths.get(1), Some(&host_bin));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn updater_progress_accumulates_chunks_and_keeps_the_first_total() {
        let mut status = DesktopUpdateStatus::Downloading {
            version: "0.3.0".into(),
            downloaded: 0,
            total: None,
        };
        add_update_chunk(&mut status, 4096, Some(16_384));
        add_update_chunk(&mut status, 2048, Some(32_768));
        assert_eq!(
            status,
            DesktopUpdateStatus::Downloading {
                version: "0.3.0".into(),
                downloaded: 6144,
                total: Some(16_384),
            }
        );
        let json = serde_json::to_value(status).unwrap();
        assert_eq!(json["phase"], "downloading");
        assert_eq!(json["downloaded"], 6144);
        assert_eq!(json["total"], 16_384);
    }

    #[test]
    fn updater_busy_and_version_projection_cover_active_phases() {
        let preparing = DesktopUpdateStatus::Preparing {
            version: Some("0.3.0".into()),
        };
        assert!(preparing.is_busy());
        assert_eq!(preparing.version().as_deref(), Some("0.3.0"));
        assert!(!DesktopUpdateStatus::Current.is_busy());
        assert_eq!(DesktopUpdateStatus::Current.version(), None);
    }

    #[test]
    fn updater_claims_separate_check_download_and_confirmed_install() {
        let mut idle = DesktopUpdateStatus::Idle;
        assert_eq!(
            claim_update_download(&mut idle).unwrap_err(),
            "no checked update available"
        );
        assert_eq!(idle, DesktopUpdateStatus::Idle);

        let mut available = DesktopUpdateStatus::Available {
            version: "0.3.0".into(),
            notes: String::new(),
        };
        assert_eq!(claim_update_download(&mut available).unwrap(), "0.3.0");
        assert_eq!(
            available,
            DesktopUpdateStatus::Preparing {
                version: Some("0.3.0".into())
            }
        );
        assert!(claim_update_check(&mut available).is_err(), "download is single-flight");

        let mut ready = DesktopUpdateStatus::Ready {
            version: "0.3.0".into(),
        };
        assert_eq!(
            claim_update_check(&mut ready).unwrap_err(),
            "downloaded update awaiting confirmation"
        );
        assert_eq!(claim_update_install(&mut ready).unwrap(), "0.3.0");
        assert_eq!(
            ready,
            DesktopUpdateStatus::Installing {
                version: "0.3.0".into()
            }
        );

        let mut failed = DesktopUpdateStatus::Failed {
            version: Some("0.3.0".into()),
            message: "offline".into(),
        };
        assert_eq!(
            claim_update_download(&mut failed).unwrap_err(),
            "no checked update available"
        );
    }

    #[test]
    fn sweep_decision_truth_table() {
        use SweepDecision::{Forget, Keep, Reap};
        assert_eq!(sweep_decision(true, true), Keep, "a live shell owns it");
        assert_eq!(sweep_decision(false, true), Reap, "orphan: shell gone, sidecar alive");
        assert_eq!(sweep_decision(true, false), Forget, "sidecar exited on its own");
        assert_eq!(sweep_decision(false, false), Forget, "both gone; stale record");
    }

    #[test]
    fn registry_add_load_and_replace_by_pid() {
        let path = scratch_registry("roundtrip");
        add_registry_entry(&path, &entry(101, 201));
        add_registry_entry(&path, &entry(102, 202));
        assert_eq!(load_registry(&path).len(), 2);

        // Re-registering the same sidecar pid replaces, never duplicates.
        let mut again = entry(101, 201);
        again.port = 39001;
        add_registry_entry(&path, &again);
        let entries = load_registry(&path);
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().any(|e| e.sidecar_pid == 101 && e.port == 39001));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn registry_load_fails_open() {
        let path = scratch_registry("fail-open");
        assert!(load_registry(&path).is_empty(), "missing file reads as empty");
        fs::write(&path, b"{not json").unwrap();
        assert!(load_registry(&path).is_empty(), "corrupt file reads as empty");
        let _ = fs::remove_file(&path);
    }

    #[test]
    #[cfg(unix)]
    fn bundled_plugin_links_runtime_owned_peers_and_profile_identity() {
        let root = scratch_dir("plugin-runtime-links");
        let runtime_cwd = root.join("runtime/dsh");
        let peer = runtime_cwd.join("node_modules/@deepseek-ai/dsh-llm");
        fs::create_dir_all(&peer).unwrap();
        let isolated = runtime_cwd.join(
            "node_modules/.pnpm/@deepseek-ai+dsh-agent@0.1.0/node_modules/@deepseek-ai/dsh-agent",
        );
        fs::create_dir_all(&isolated).unwrap();
        let runtime = Runtime {
            node: PathBuf::from("node"),
            args_prefix: Vec::new(),
            cli: PathBuf::from("dsh"),
            cwd: runtime_cwd,
            path_prepend: Vec::new(),
        };
        assert_eq!(
            fs::canonicalize(resolve_runtime_package(&runtime, "@deepseek-ai/dsh-agent").unwrap()).unwrap(),
            fs::canonicalize(&isolated).unwrap()
        );

        let plugin = root.join("plugin");
        fs::create_dir_all(&plugin).unwrap();
        fs::write(plugin.join(".ok"), "hash\n").unwrap();
        ensure_bundled_plugin_runtime_links(&plugin, &runtime, &["@deepseek-ai/dsh-llm"]).unwrap();
        assert_eq!(
            fs::canonicalize(plugin.join("node_modules/@deepseek-ai/dsh-llm")).unwrap(),
            fs::canonicalize(peer).unwrap()
        );

        let dsh_home = root.join("home");
        let profile_link = dsh_home.join("profiles/web/node_modules").join(COMPACTION_PACKAGE);
        fs::create_dir_all(profile_link.parent().unwrap()).unwrap();
        link_dir(&plugin, &profile_link).unwrap();
        assert!(plugin_already_in_profile(&plugin, COMPACTION_PACKAGE, &dsh_home));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn ps_lstart_matches_only_the_same_process_instance() {
        let pid = std::process::id();
        let lstart = ps_lstart(pid).expect("ps must see this test process");
        assert!(pid_matches(pid, &lstart), "fresh lstart matches");
        assert!(!pid_matches(pid, "recorded-before-a-reboot"), "stale lstart reads as dead");
    }

    #[test]
    #[cfg(unix)]
    fn term_then_kill_terminates_a_scratch_process() {
        let mut child = std::process::Command::new("sleep").arg("30").spawn().expect("spawn sleep");
        let pid = child.id();
        // `sleep` dies on SIGTERM immediately, so the ladder returns fast.
        term_then_kill(pid);
        let status = child.wait().expect("wait for sleep");
        assert!(!status.success(), "the scratch process must be terminated, not alive");
        assert!(ps_lstart(pid).is_none(), "the pid must be gone after the ladder");
    }

    /// The coverage contract of the process-group design: one group signal
    /// must reach the sidecar's own children (the "grandchildren" from the
    /// shell's point of view), with no tree enumeration.
    #[test]
    #[cfg(unix)]
    fn process_group_signal_reaches_grandchildren() {
        use std::os::unix::process::CommandExt as _;
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg("sleep 30 & sleep 30 & wait")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .expect("spawn sh");
        let pid = child.id();
        // The sidecar spawn pattern: the child leads its own group, so the
        // signal target is the group, not the pid.
        assert_eq!(unsafe { libc::getpgid(pid as libc::pid_t) }, pid as libc::pid_t);
        assert_eq!(signal_target(pid), -(pid as libc::pid_t));
        // Wait until the group really holds sh + the two sleeps.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let out = std::process::Command::new("pgrep")
                .arg("-g")
                .arg(pid.to_string())
                .output()
                .expect("pgrep");
            let alive = String::from_utf8_lossy(&out.stdout).lines().filter(|l| !l.trim().is_empty()).count();
            if alive >= 3 || Instant::now() > deadline {
                assert!(alive >= 3, "expected sh + 2 sleeps in the group, saw {alive}");
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        // One atomic group signal — no per-pid enumeration anywhere.
        unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGTERM) };
        let _ = child.wait();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let out = std::process::Command::new("pgrep")
                .arg("-g")
                .arg(pid.to_string())
                .output()
                .expect("pgrep");
            let alive = String::from_utf8_lossy(&out.stdout).lines().filter(|l| !l.trim().is_empty()).count();
            if alive == 0 {
                break;
            }
            assert!(Instant::now() < deadline, "{alive} grandchildren survived the group signal");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    #[cfg(windows)]
    fn ps_lstart_matches_only_the_same_process_instance() {
        let pid = std::process::id();
        let lstart = ps_lstart(pid).expect("GetProcessTimes must see this test process");
        assert!(pid_matches(pid, &lstart), "fresh token matches");
        assert!(!pid_matches(pid, "0"), "stale token reads as dead");
    }

    #[test]
    #[cfg(windows)]
    fn term_then_kill_terminates_a_scratch_process() {
        let mut command = std::process::Command::new("ping");
        command.args(["-n", "30", "127.0.0.1"]).stdout(Stdio::null()).stderr(Stdio::null());
        hide_console(&mut command);
        let mut child = command.spawn().expect("spawn ping");
        let pid = child.id();
        term_then_kill(pid);
        let _ = child.wait();
        assert!(ps_lstart(pid).is_none(), "the pid must be gone after the ladder");
    }
}
