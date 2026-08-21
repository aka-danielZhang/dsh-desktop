//! Staged web-profile mutation for desktop-owned package installation.
//!
//! pnpm install/add can rewrite the manifest, lockfile, and node_modules. Run
//! those commands against a sibling shadow DSH_HOME, then promote the complete
//! profile only after validation. The sibling topology preserves relative
//! file:/link: depth, and a journal recovers every interrupted rename phase.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const PROFILE_NAME: &str = "web";
const JOURNAL_NAME: &str = ".desktop-profile-repair.json";
const MARKER_NAME: &str = ".dsh-desktop-profile-transaction";
const JOURNAL_SCHEMA: u8 = 1;
const RENAME_ATTEMPTS: usize = 5;
const RENAME_RETRY_DELAY: Duration = Duration::from_millis(50);

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
enum RepairPhase {
    Prepared,
    ShadowReady,
    OriginalMoved,
    ShadowPromoted,
    RollingBack,
    Aborted,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct RepairTarget {
    package: String,
    source: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct RepairJournal {
    schema: u8,
    owner_pid: u32,
    owner_lstart: String,
    created_unix_ms: u64,
    id: String,
    phase: RepairPhase,
    had_original: bool,
    real_profile: PathBuf,
    shadow_profile: PathBuf,
    backup_profile: PathBuf,
    targets: Vec<RepairTarget>,
    original_identity: Option<String>,
    home_patch_identity: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct PhaseRecord {
    schema: u8,
    id: String,
    phase: RepairPhase,
}

struct RepairPaths {
    journal: PathBuf,
    profile: PathBuf,
    backup: PathBuf,
    shadow_home: PathBuf,
    shadow_profile: PathBuf,
}

#[derive(Debug, PartialEq)]
enum IdentityEntry {
    File(Vec<u8>),
    Directory,
    Symlink(PathBuf),
}

type ProfileIdentity = BTreeMap<PathBuf, IdentityEntry>;

pub(super) fn recover_web_profile(dsh_home: &Path) -> Result<(), String> {
    recover_stale_repair(dsh_home)
}

/// Mutate a complete shadow copy of the web profile and promote it as one
/// transaction. The closure receives the shadow DSH_HOME and whether a real
/// profile existed. All desktop-owned packages belong in this one closure.
pub(super) fn mutate_web_profile<F>(
    dsh_home: &Path,
    targets: &[(&str, &Path)],
    mutate: F,
) -> Result<(), String>
where
    F: FnOnce(&Path, bool) -> Result<(), String>,
{
    fs::create_dir_all(dsh_home)
        .map_err(|error| format!("create {}: {error}", dsh_home.display()))?;
    recover_stale_repair(dsh_home)?;

    let profile = dsh_home.join("profiles").join(PROFILE_NAME);
    if profile.exists() && !profile.is_dir() {
        return Err(format!(
            "web profile path is not a directory: {}",
            profile.display()
        ));
    }
    let had_original = profile.is_dir();
    let original_identity = if had_original {
        Some(capture_profile_identity(&profile)?)
    } else {
        None
    };
    let home_patch = dsh_home.join("cordis.patch.yml");
    let original_home_patch = read_optional_file(&home_patch)?;
    let id = transaction_id()?;
    let paths = repair_paths(dsh_home, &id)?;
    let owner_pid = std::process::id();
    let owner_lstart = super::ps_lstart(owner_pid)
        .ok_or_else(|| format!("cannot identify profile repair owner pid {owner_pid}"))?;
    let created_unix_ms = unix_millis()?;
    let journal_targets = targets
        .iter()
        .map(|(package, source)| {
            fs::canonicalize(source)
                .map(|source| RepairTarget {
                    package: (*package).to_string(),
                    source,
                })
                .map_err(|error| {
                    format!(
                        "resolve desktop-owned package {package} at {}: {error}",
                        source.display()
                    )
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut journal = RepairJournal {
        schema: JOURNAL_SCHEMA,
        owner_pid,
        owner_lstart,
        created_unix_ms,
        id,
        phase: RepairPhase::Prepared,
        had_original,
        real_profile: paths.profile.clone(),
        shadow_profile: paths.shadow_profile.clone(),
        backup_profile: paths.backup.clone(),
        targets: journal_targets,
        original_identity: original_identity.as_ref().map(identity_fingerprint),
        home_patch_identity: original_home_patch.as_deref().map(bytes_fingerprint),
    };
    write_new_journal(&paths.journal, &journal)?;

    let staged = (|| {
        if had_original {
            copy_profile_tree(&paths.profile, &paths.shadow_profile)?;
        }
        if let Some(bytes) = &original_home_patch {
            fs::create_dir_all(&paths.shadow_home)
                .map_err(|error| format!("create {}: {error}", paths.shadow_home.display()))?;
            fs::write(paths.shadow_home.join("cordis.patch.yml"), bytes)
                .map_err(|error| format!("copy home cordis.patch.yml into staging: {error}"))?;
        }
        mutate(&paths.shadow_home, had_original)?;
        validate_staged_profile(&paths.shadow_profile)?;
        validate_profile_targets(&paths.shadow_profile, &journal.targets)?;
        let current_identity = if paths.profile.is_dir() {
            Some(capture_profile_identity(&paths.profile)?)
        } else {
            None
        };
        if current_identity != original_identity {
            return Err("web profile changed outside the desktop repair transaction".to_string());
        }
        if read_optional_file(&home_patch)? != original_home_patch {
            return Err(
                "home cordis.patch.yml changed outside the desktop repair transaction".to_string(),
            );
        }
        write_marker(&paths.shadow_profile, &journal.id)?;
        journal.phase = RepairPhase::ShadowReady;
        update_journal(&paths.journal, &journal)?;
        Ok(())
    })();
    if let Err(error) = staged {
        return rollback_before_commit(&paths, error);
    }

    fs::create_dir_all(paths.profile.parent().expect("profile always has a parent")).map_err(
        |error| {
            rollback_before_commit(&paths, format!("create profile parent: {error}")).unwrap_err()
        },
    )?;
    if had_original {
        rename_with_retry(&paths.profile, &paths.backup).map_err(|error| {
            rollback_before_commit(
                &paths,
                format!(
                    "stage existing profile {} as {}: {error}",
                    paths.profile.display(),
                    paths.backup.display()
                ),
            )
            .unwrap_err()
        })?;
        if let Err(error) = validate_original_identity(
            &paths.backup,
            original_identity
                .as_ref()
                .expect("original profile has an identity"),
        )
        .and_then(|()| validate_optional_file_unchanged(&home_patch, &original_home_patch))
        {
            if let Err(restore_error) = rename_with_retry(&paths.backup, &paths.profile) {
                return Err(format!(
                    "{error}; restore {} failed: {restore_error}; journal retained at {}",
                    paths.profile.display(),
                    paths.journal.display()
                ));
            }
            return rollback_before_commit(&paths, error);
        }
        journal.phase = RepairPhase::OriginalMoved;
        let phase_result =
            sync_directory(paths.profile.parent().expect("profile always has a parent"))
                .and_then(|()| update_journal(&paths.journal, &journal));
        if let Err(error) = phase_result {
            if let Err(restore_error) = rename_with_retry(&paths.backup, &paths.profile) {
                return Err(format!(
                    "record original-moved phase failed: {error}; restore {} failed: {restore_error}; journal retained at {}",
                    paths.profile.display(),
                    paths.journal.display()
                ));
            }
            return rollback_before_commit(
                &paths,
                format!("record original-moved phase failed: {error}"),
            );
        }
    }
    if let Err(error) = rename_with_retry(&paths.shadow_profile, &paths.profile) {
        let promote_error = format!(
            "promote staged profile {} to {}: {error}",
            paths.shadow_profile.display(),
            paths.profile.display()
        );
        if had_original {
            if let Err(restore_error) = rename_with_retry(&paths.backup, &paths.profile) {
                return Err(format!(
                    "{promote_error}; restore {} failed: {restore_error}; journal retained at {}",
                    paths.profile.display(),
                    paths.journal.display()
                ));
            }
        }
        return rollback_before_commit(&paths, promote_error);
    }
    journal.phase = RepairPhase::ShadowPromoted;
    let promoted_phase =
        sync_directory(paths.profile.parent().expect("profile always has a parent"))
            .and_then(|()| update_journal(&paths.journal, &journal));
    if let Err(error) = promoted_phase {
        return rollback_after_promotion(
            &paths,
            had_original,
            format!("record shadow-promoted phase failed: {error}"),
        );
    }

    // New profile is live. Keep the journal and marker until destructive
    // cleanup finishes; the next boot can prove which profile was promoted.
    if let Err(error) = validate_staged_profile(&paths.profile)
        .and_then(|()| validate_profile_targets(&paths.profile, &journal.targets))
    {
        return rollback_after_promotion(&paths, had_original, error);
    }
    if had_original {
        if let Err(error) = validate_original_identity(
            &paths.backup,
            original_identity
                .as_ref()
                .expect("original profile has an identity"),
        ) {
            return rollback_after_promotion(&paths, true, error);
        }
    }
    if let Err(error) = validate_optional_file_unchanged(&home_patch, &original_home_patch) {
        return rollback_after_promotion(&paths, had_original, error);
    }
    if paths.backup.exists() {
        remove_path(&paths.backup).map_err(|error| {
            format!(
                "profile committed but remove backup {} failed: {error}; journal retained at {}",
                paths.backup.display(),
                paths.journal.display()
            )
        })?;
    }
    remove_path_if_exists(&paths.shadow_home).map_err(|error| {
        format!(
            "profile committed but remove shadow home {} failed: {error}; journal retained at {}",
            paths.shadow_home.display(),
            paths.journal.display()
        )
    })?;
    fs::remove_file(paths.profile.join(MARKER_NAME))
        .map_err(|error| format!("remove committed profile marker: {error}"))?;
    remove_journal_records(&paths.journal)?;
    Ok(())
}

fn recover_stale_repair(dsh_home: &Path) -> Result<(), String> {
    let journal_path = dsh_home.join(JOURNAL_NAME);
    if !journal_path.exists() {
        let marker = dsh_home
            .join("profiles")
            .join(PROFILE_NAME)
            .join(MARKER_NAME);
        if marker.exists() {
            return Err(format!(
                "profile transaction marker exists without a journal: {}; preserving it for manual recovery",
                marker.display()
            ));
        }
        return Ok(());
    }
    let text = fs::read_to_string(&journal_path).map_err(|error| {
        format!(
            "read profile repair journal {}: {error}",
            journal_path.display()
        )
    })?;
    let mut journal: RepairJournal = serde_json::from_str(&text).map_err(|error| {
        format!(
            "parse profile repair journal {}: {error}",
            journal_path.display()
        )
    })?;
    validate_journal(dsh_home, &journal)?;
    if super::pid_matches(journal.owner_pid, &journal.owner_lstart) {
        return Err(format!(
            "web profile repair already owned by live process {}",
            journal.owner_pid
        ));
    }
    journal.phase = read_durable_phase(&journal_path, &journal)?;

    let paths = repair_paths(dsh_home, &journal.id)?;
    let real = paths.profile.exists();
    let backup = paths.backup.exists();
    let shadow = paths.shadow_profile.exists();
    let marker = read_marker(&paths.profile)?;
    if marker.as_deref().is_some_and(|value| value != journal.id) {
        return Err(format!(
            "profile marker does not match repair {}; preserving all paths",
            journal.id
        ));
    }
    let promoted = marker.as_deref() == Some(journal.id.as_str());
    let state = (real, backup, shadow, promoted);

    match (&journal.phase, journal.had_original, state) {
        (RepairPhase::Prepared, true, (true, false, _, false))
        | (RepairPhase::ShadowReady, true, (true, false, _, false)) => {
            discard_shadow(&paths)?;
        }
        (RepairPhase::Aborted, true, (true, false, _, false)) => {
            validate_original_fingerprint(&paths.profile, &journal)?;
            discard_shadow(&paths)?;
        }
        (RepairPhase::Prepared, false, (false, false, _, false))
        | (RepairPhase::ShadowReady, false, (false, false, _, false))
        | (RepairPhase::Aborted, false, (false, false, _, false)) => {
            discard_shadow(&paths)?;
        }
        (RepairPhase::ShadowReady, true, (false, true, _, false))
        | (RepairPhase::OriginalMoved, true, (false, true, _, false))
        | (RepairPhase::RollingBack, true, (false, true, _, false)) => {
            restore_backup(&paths, &journal)?;
        }
        (RepairPhase::RollingBack, true, (true, true, false, true)) => {
            roll_back_live_candidate(&paths, &journal, true)?;
        }
        (RepairPhase::RollingBack, true, (true, false, true, false)) => {
            validate_original_fingerprint(&paths.profile, &journal)?;
            discard_shadow(&paths)?;
        }
        (RepairPhase::RollingBack, false, (true, false, false, true)) => {
            roll_back_live_candidate(&paths, &journal, false)?;
        }
        (RepairPhase::RollingBack, false, (false, false, true, false)) => {
            discard_shadow(&paths)?;
        }
        // New-profile promotion has no original-moved phase.
        (RepairPhase::ShadowReady, false, (true, false, false, true)) => {
            resolve_promoted_recovery(dsh_home, &paths, &journal)?;
        }
        // Existing-profile promotion can complete before ShadowPromoted is
        // durably recorded, but only with the matching marker and backup.
        (RepairPhase::OriginalMoved, true, (true, true, false, true)) => {
            resolve_promoted_recovery(dsh_home, &paths, &journal)?;
        }
        // Cleanup can remove backup and marker after ShadowPromoted; that
        // phase is the durable commit record for those later states.
        (RepairPhase::ShadowPromoted, true, (true, true, false, true))
        | (RepairPhase::ShadowPromoted, true, (true, false, false, true))
        | (RepairPhase::ShadowPromoted, true, (true, false, false, false))
        | (RepairPhase::ShadowPromoted, false, (true, false, false, true))
        | (RepairPhase::ShadowPromoted, false, (true, false, false, false)) => {
            resolve_promoted_recovery(dsh_home, &paths, &journal)?;
        }
        _ => {
            return Err(format!(
                "ambiguous profile repair {} in phase {:?}: hadOriginal={}, real/backup/shadow/promoted={state:?}; preserving all paths",
                journal.id, journal.phase, journal.had_original
            ));
        }
    }
    remove_journal_records(&paths.journal)?;
    Ok(())
}

fn discard_shadow(paths: &RepairPaths) -> Result<(), String> {
    remove_path_if_exists(&paths.shadow_home).map_err(|error| {
        format!(
            "remove abandoned shadow home {}: {error}",
            paths.shadow_home.display()
        )
    })
}

fn restore_backup(paths: &RepairPaths, journal: &RepairJournal) -> Result<(), String> {
    validate_original_fingerprint(&paths.backup, journal)?;
    rename_with_retry(&paths.backup, &paths.profile).map_err(|error| {
        format!(
            "restore interrupted profile {} from {}: {error}",
            paths.profile.display(),
            paths.backup.display()
        )
    })?;
    remove_path_if_exists(&paths.shadow_home).map_err(|error| {
        format!(
            "remove rolled-back shadow home {}: {error}",
            paths.shadow_home.display()
        )
    })
}

fn roll_back_live_candidate(
    paths: &RepairPaths,
    journal: &RepairJournal,
    had_original: bool,
) -> Result<(), String> {
    rename_with_retry(&paths.profile, &paths.shadow_profile).map_err(|error| {
        format!(
            "move promoted profile {} back to {}: {error}",
            paths.profile.display(),
            paths.shadow_profile.display()
        )
    })?;
    if had_original {
        restore_backup(paths, journal)
    } else {
        discard_shadow(paths)
    }
}

fn validate_original_fingerprint(profile: &Path, journal: &RepairJournal) -> Result<(), String> {
    let expected = journal
        .original_identity
        .as_deref()
        .ok_or_else(|| "repair journal lacks original profile fingerprint".to_string())?;
    let actual = identity_fingerprint(&capture_profile_identity(profile)?);
    if actual == expected {
        Ok(())
    } else {
        Err(format!(
            "original profile fingerprint changed at {}; preserving transaction paths",
            profile.display()
        ))
    }
}

fn resolve_promoted_recovery(
    dsh_home: &Path,
    paths: &RepairPaths,
    journal: &RepairJournal,
) -> Result<(), String> {
    if home_patch_matches(dsh_home, journal)? || (journal.had_original && !paths.backup.exists()) {
        finish_promoted_recovery(paths, journal)
    } else {
        roll_back_live_candidate(paths, journal, journal.had_original)
    }
}

fn finish_promoted_recovery(paths: &RepairPaths, journal: &RepairJournal) -> Result<(), String> {
    validate_staged_profile(&paths.profile)?;
    validate_profile_targets(&paths.profile, &journal.targets)?;
    if paths.backup.exists() {
        validate_original_fingerprint(&paths.backup, journal)?;
        validate_staged_profile(&paths.profile)?;
        validate_profile_targets(&paths.profile, &journal.targets)?;
        remove_path(&paths.backup).map_err(|error| {
            format!(
                "remove committed profile backup {}: {error}",
                paths.backup.display()
            )
        })?;
    }
    remove_path_if_exists(&paths.shadow_home).map_err(|error| {
        format!(
            "remove committed shadow home {}: {error}",
            paths.shadow_home.display()
        )
    })?;
    remove_path_if_exists(&paths.profile.join(MARKER_NAME))
        .map_err(|error| format!("remove recovered profile marker: {error}"))?;
    Ok(())
}

fn validate_journal(dsh_home: &Path, journal: &RepairJournal) -> Result<(), String> {
    if journal.schema != JOURNAL_SCHEMA {
        return Err(format!(
            "unsupported profile repair journal schema {}",
            journal.schema
        ));
    }
    validate_id(&journal.id)?;
    let paths = repair_paths(dsh_home, &journal.id)?;
    if journal.real_profile != paths.profile
        || journal.shadow_profile != paths.shadow_profile
        || journal.backup_profile != paths.backup
    {
        return Err("profile repair journal paths do not match DSH_HOME".to_string());
    }
    if journal.had_original != journal.original_identity.is_some() {
        return Err("profile repair journal original identity is inconsistent".to_string());
    }
    if journal
        .targets
        .iter()
        .any(|target| target.package.is_empty() || !target.source.is_absolute())
    {
        return Err("profile repair journal contains an invalid target".to_string());
    }
    Ok(())
}

fn validate_staged_profile(profile: &Path) -> Result<(), String> {
    for required in ["package.json", "cordis.patch.yml", "pnpm-workspace.yaml"] {
        if !profile.join(required).is_file() {
            return Err(format!(
                "staged web profile lacks {required}: {}",
                profile.display()
            ));
        }
    }
    if !profile.join("node_modules").is_dir() {
        return Err(format!(
            "staged web profile lacks node_modules: {}",
            profile.display()
        ));
    }
    Ok(())
}

fn validate_profile_targets(profile: &Path, targets: &[RepairTarget]) -> Result<(), String> {
    if targets.is_empty() {
        return Ok(());
    }
    let manifest_path = profile.join("package.json");
    let manifest_text = fs::read_to_string(&manifest_path)
        .map_err(|error| format!("read staged manifest {}: {error}", manifest_path.display()))?;
    let manifest: serde_json::Value = serde_json::from_str(&manifest_text)
        .map_err(|error| format!("parse staged manifest {}: {error}", manifest_path.display()))?;
    let dependencies = manifest
        .get("dependencies")
        .and_then(|value| value.as_object())
        .ok_or_else(|| {
            format!(
                "staged manifest lacks dependencies: {}",
                manifest_path.display()
            )
        })?;
    for target in targets {
        if !dependencies.contains_key(&target.package) {
            return Err(format!(
                "staged manifest lacks dependency {}",
                target.package
            ));
        }
        let actual = fs::canonicalize(profile.join("node_modules").join(&target.package))
            .map_err(|error| format!("resolve staged {}: {error}", target.package))?;
        if actual != target.source {
            return Err(format!(
                "staged {} resolves to {}, expected {}",
                target.package,
                actual.display(),
                target.source.display()
            ));
        }
    }
    Ok(())
}

fn write_marker(profile: &Path, id: &str) -> Result<(), String> {
    let marker = profile.join(MARKER_NAME);
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&marker)
        .map_err(|error| format!("create staged profile marker {}: {error}", marker.display()))?;
    file.write_all(format!("{id}\n").as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("write staged profile marker {}: {error}", marker.display()))?;
    sync_directory(profile)
}

fn read_marker(profile: &Path) -> Result<Option<String>, String> {
    let marker = profile.join(MARKER_NAME);
    if !marker.exists() {
        return Ok(None);
    }
    let value = fs::read_to_string(&marker).map_err(|error| {
        format!(
            "read profile transaction marker {}: {error}",
            marker.display()
        )
    })?;
    Ok(Some(value.trim().to_string()))
}

fn rollback_after_promotion(
    paths: &RepairPaths,
    had_original: bool,
    error: String,
) -> Result<(), String> {
    if let Err(phase_error) = mark_journal_phase(&paths.journal, RepairPhase::RollingBack) {
        return Err(format!(
            "{error}; record rolling-back phase failed: {phase_error}; promoted profile and backup retained"
        ));
    }
    if let Err(move_error) = rename_with_retry(&paths.profile, &paths.shadow_profile) {
        return Err(format!(
            "{error}; move promoted profile back to staging failed: {move_error}; journal retained at {}",
            paths.journal.display()
        ));
    }
    if had_original {
        if let Err(restore_error) = rename_with_retry(&paths.backup, &paths.profile) {
            return Err(format!(
                "{error}; restore original profile failed: {restore_error}; journal retained at {}",
                paths.journal.display()
            ));
        }
    }
    rollback_before_commit(paths, error)
}

fn rollback_before_commit(paths: &RepairPaths, error: String) -> Result<(), String> {
    if let Err(abort_error) = mark_journal_aborted(&paths.journal) {
        return Err(format!(
            "{error}; record aborted phase failed: {abort_error}; staging and journal retained"
        ));
    }
    let mut failures = Vec::new();
    if let Err(cleanup_error) = remove_path_if_exists(&paths.shadow_home) {
        failures.push(format!("remove shadow home: {cleanup_error}"));
    }
    if failures.is_empty() {
        if let Err(cleanup_error) = remove_journal_records(&paths.journal) {
            failures.push(format!("remove journal records: {cleanup_error}"));
        }
    }
    if failures.is_empty() {
        Err(error)
    } else {
        Err(format!("{error}; cleanup failed: {}", failures.join("; ")))
    }
}

fn mark_journal_aborted(path: &Path) -> Result<(), String> {
    mark_journal_phase(path, RepairPhase::Aborted)
}

fn mark_journal_phase(path: &Path, phase: RepairPhase) -> Result<(), String> {
    let text = fs::read_to_string(path).map_err(|error| {
        format!(
            "read journal before phase update {}: {error}",
            path.display()
        )
    })?;
    let mut journal: RepairJournal = serde_json::from_str(&text).map_err(|error| {
        format!(
            "parse journal before phase update {}: {error}",
            path.display()
        )
    })?;
    journal.phase = phase;
    update_journal(path, &journal)
}

fn repair_paths(dsh_home: &Path, id: &str) -> Result<RepairPaths, String> {
    validate_id(id)?;
    let parent = dsh_home
        .parent()
        .ok_or_else(|| format!("DSH_HOME has no parent: {}", dsh_home.display()))?;
    let name = dsh_home
        .file_name()
        .ok_or_else(|| format!("DSH_HOME has no final component: {}", dsh_home.display()))?
        .to_string_lossy();
    // Sibling DSH homes keep profiles/web at identical depth and on the same
    // volume, preserving pnpm's relative file:/link: specs and renameability.
    let shadow_home = parent.join(format!(".{name}-desktop-profile-repair-{id}"));
    let profiles = dsh_home.join("profiles");
    Ok(RepairPaths {
        journal: dsh_home.join(JOURNAL_NAME),
        profile: profiles.join(PROFILE_NAME),
        backup: profiles.join(format!(".{PROFILE_NAME}-desktop-backup-{id}")),
        shadow_profile: shadow_home.join("profiles").join(PROFILE_NAME),
        shadow_home,
    })
}

fn transaction_id() -> Result<String, String> {
    Ok(format!("{}-{}", std::process::id(), unix_millis()?))
}

fn unix_millis() -> Result<u64, String> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("read system time for profile repair: {error}"))?
        .as_millis();
    u64::try_from(millis).map_err(|_| "system time does not fit u64 milliseconds".to_string())
}

fn validate_id(id: &str) -> Result<(), String> {
    if !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit() || byte == b'-') {
        Ok(())
    } else {
        Err(format!("invalid profile repair id {id:?}"))
    }
}

fn write_new_journal(path: &Path, journal: &RepairJournal) -> Result<(), String> {
    let requested_phase = journal.phase;
    let mut immutable = journal.clone();
    immutable.phase = RepairPhase::Prepared;
    let bytes = serde_json::to_vec_pretty(&immutable)
        .map_err(|error| format!("serialize profile repair journal: {error}"))?;
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .map_err(|error| format!("create profile repair journal {}: {error}", path.display()))?;
    if let Err(error) = file
        .write_all(&bytes)
        .and_then(|_| file.write_all(b"\n"))
        .and_then(|_| file.sync_all())
    {
        let _ = fs::remove_file(path);
        return Err(format!(
            "write profile repair journal {}: {error}",
            path.display()
        ));
    }
    sync_directory(path.parent().expect("journal always has a parent"))?;
    if requested_phase != RepairPhase::Prepared {
        record_phase(path, &journal.id, requested_phase)?;
    }
    Ok(())
}

fn update_journal(path: &Path, journal: &RepairJournal) -> Result<(), String> {
    record_phase(path, &journal.id, journal.phase)
}

fn record_phase(path: &Path, id: &str, phase: RepairPhase) -> Result<(), String> {
    if phase == RepairPhase::Prepared {
        return Ok(());
    }
    let final_path = phase_record_path(path, id, phase)?;
    if final_path.exists() {
        return validate_phase_record(&final_path, id, phase);
    }
    let record = PhaseRecord {
        schema: JOURNAL_SCHEMA,
        id: id.to_string(),
        phase,
    };
    let bytes = serde_json::to_vec_pretty(&record)
        .map_err(|error| format!("serialize profile repair phase: {error}"))?;
    let temp = final_path.with_extension(format!("phase.{}.tmp", std::process::id()));
    remove_path_if_exists(&temp)
        .map_err(|error| format!("remove stale phase temp {}: {error}", temp.display()))?;
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temp)
        .map_err(|error| format!("create profile repair phase {}: {error}", temp.display()))?;
    if let Err(error) = file
        .write_all(&bytes)
        .and_then(|_| file.write_all(b"\n"))
        .and_then(|_| file.sync_all())
    {
        let _ = fs::remove_file(&temp);
        return Err(format!(
            "write profile repair phase {}: {error}",
            temp.display()
        ));
    }
    if let Err(error) = fs::rename(&temp, &final_path) {
        if final_path.exists() && validate_phase_record(&final_path, id, phase).is_ok() {
            let _ = fs::remove_file(&temp);
            return Ok(());
        }
        return Err(format!(
            "publish profile repair phase {}: {error}",
            final_path.display()
        ));
    }
    sync_directory(path.parent().expect("journal always has a parent"))
}

fn phase_record_path(path: &Path, id: &str, phase: RepairPhase) -> Result<PathBuf, String> {
    validate_id(id)?;
    Ok(path
        .parent()
        .expect("journal always has a parent")
        .join(format!(
            ".desktop-profile-repair.{id}.{}.phase",
            phase_slug(phase)
        )))
}

fn phase_slug(phase: RepairPhase) -> &'static str {
    match phase {
        RepairPhase::Prepared => "prepared",
        RepairPhase::ShadowReady => "shadow-ready",
        RepairPhase::OriginalMoved => "original-moved",
        RepairPhase::ShadowPromoted => "shadow-promoted",
        RepairPhase::RollingBack => "rolling-back",
        RepairPhase::Aborted => "aborted",
    }
}

fn durable_phases() -> [RepairPhase; 5] {
    [
        RepairPhase::ShadowReady,
        RepairPhase::OriginalMoved,
        RepairPhase::ShadowPromoted,
        RepairPhase::RollingBack,
        RepairPhase::Aborted,
    ]
}

fn validate_phase_record(path: &Path, id: &str, phase: RepairPhase) -> Result<(), String> {
    let text = fs::read_to_string(path)
        .map_err(|error| format!("read profile repair phase {}: {error}", path.display()))?;
    let record: PhaseRecord = serde_json::from_str(&text)
        .map_err(|error| format!("parse profile repair phase {}: {error}", path.display()))?;
    if record.schema != JOURNAL_SCHEMA || record.id != id || record.phase != phase {
        return Err(format!(
            "profile repair phase record mismatch: {}",
            path.display()
        ));
    }
    Ok(())
}

fn read_durable_phase(path: &Path, journal: &RepairJournal) -> Result<RepairPhase, String> {
    let mut phase = journal.phase;
    for candidate in durable_phases() {
        let record_path = phase_record_path(path, &journal.id, candidate)?;
        if record_path.exists() {
            validate_phase_record(&record_path, &journal.id, candidate)?;
            phase = candidate;
        }
    }
    Ok(phase)
}

fn remove_journal_records(path: &Path) -> Result<(), String> {
    let text = fs::read_to_string(path)
        .map_err(|error| format!("read journal before cleanup {}: {error}", path.display()))?;
    let journal: RepairJournal = serde_json::from_str(&text)
        .map_err(|error| format!("parse journal before cleanup {}: {error}", path.display()))?;
    let active = read_durable_phase(path, &journal)?;
    for phase in durable_phases() {
        if phase == active {
            continue;
        }
        let record = phase_record_path(path, &journal.id, phase)?;
        remove_path_if_exists(&record).map_err(|error| {
            format!("remove profile repair phase {}: {error}", record.display())
        })?;
    }
    let parent = path.parent().expect("journal always has a parent");
    sync_directory(parent)?;
    fs::remove_file(path)
        .map_err(|error| format!("remove profile repair journal {}: {error}", path.display()))?;
    sync_directory(parent)?;
    if active != RepairPhase::Prepared {
        let record = phase_record_path(path, &journal.id, active)?;
        if let Err(error) = remove_path_if_exists(&record) {
            eprintln!(
                "dsh-desktop: completed profile repair left harmless phase record {}: {error}",
                record.display()
            );
        }
        let _ = sync_directory(parent);
    }
    Ok(())
}

fn validate_original_identity(profile: &Path, expected: &ProfileIdentity) -> Result<(), String> {
    let actual = capture_profile_identity(profile)?;
    if &actual == expected {
        Ok(())
    } else {
        Err("web profile changed during desktop transaction commit".to_string())
    }
}

fn read_optional_file(path: &Path) -> Result<Option<Vec<u8>>, String> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("read optional file {}: {error}", path.display())),
    }
}

fn home_patch_matches(dsh_home: &Path, journal: &RepairJournal) -> Result<bool, String> {
    let actual = read_optional_file(&dsh_home.join("cordis.patch.yml"))?
        .as_deref()
        .map(bytes_fingerprint);
    Ok(actual == journal.home_patch_identity)
}

fn validate_optional_file_unchanged(path: &Path, expected: &Option<Vec<u8>>) -> Result<(), String> {
    if &read_optional_file(path)? == expected {
        Ok(())
    } else {
        Err(format!(
            "{} changed during desktop transaction commit",
            path.display()
        ))
    }
}

fn capture_profile_identity(profile: &Path) -> Result<ProfileIdentity, String> {
    let mut identity = BTreeMap::new();
    capture_tree(profile, profile, &mut identity)?;
    Ok(identity)
}

fn capture_tree(root: &Path, current: &Path, identity: &mut ProfileIdentity) -> Result<(), String> {
    let entries =
        fs::read_dir(current).map_err(|error| format!("read {}: {error}", current.display()))?;
    for entry in entries {
        let entry =
            entry.map_err(|error| format!("read entry under {}: {error}", current.display()))?;
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .map_err(|error| format!("relative profile path {}: {error}", path.display()))?
            .to_path_buf();
        let file_type = entry
            .file_type()
            .map_err(|error| format!("read file type {}: {error}", path.display()))?;
        if file_type.is_dir() {
            identity.insert(relative, directory_identity(&path)?);
            if current == root && entry.file_name() == OsStr::new("node_modules") {
                capture_node_modules_top(root, &path, identity)?;
            } else {
                capture_tree(root, &path, identity)?;
            }
        } else if file_type.is_file() {
            let bytes =
                fs::read(&path).map_err(|error| format!("read {}: {error}", path.display()))?;
            identity.insert(relative, IdentityEntry::File(bytes));
        } else if file_type.is_symlink() {
            let link = fs::read_link(&path)
                .map_err(|error| format!("read symlink {}: {error}", path.display()))?;
            identity.insert(relative, IdentityEntry::Symlink(link));
        } else {
            return Err(format!(
                "unsupported profile entry type: {}",
                path.display()
            ));
        }
    }
    Ok(())
}

fn capture_node_modules_top(
    root: &Path,
    node_modules: &Path,
    identity: &mut ProfileIdentity,
) -> Result<(), String> {
    let entries = fs::read_dir(node_modules)
        .map_err(|error| format!("read {}: {error}", node_modules.display()))?;
    for entry in entries {
        let entry = entry
            .map_err(|error| format!("read entry under {}: {error}", node_modules.display()))?;
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .map_err(|error| format!("relative profile path {}: {error}", path.display()))?
            .to_path_buf();
        let file_type = entry
            .file_type()
            .map_err(|error| format!("read file type {}: {error}", path.display()))?;
        let value = if file_type.is_symlink() {
            IdentityEntry::Symlink(
                fs::read_link(&path)
                    .map_err(|error| format!("read symlink {}: {error}", path.display()))?,
            )
        } else if file_type.is_file() {
            IdentityEntry::File(
                fs::read(&path).map_err(|error| format!("read {}: {error}", path.display()))?,
            )
        } else if file_type.is_dir() {
            let value = directory_identity(&path)?;
            if entry.file_name().to_string_lossy().starts_with('@') {
                capture_node_modules_scope(root, &path, identity)?;
            }
            value
        } else {
            return Err(format!(
                "unsupported node_modules entry type: {}",
                path.display()
            ));
        };
        identity.insert(relative, value);
    }
    Ok(())
}

fn capture_node_modules_scope(
    root: &Path,
    scope: &Path,
    identity: &mut ProfileIdentity,
) -> Result<(), String> {
    let entries =
        fs::read_dir(scope).map_err(|error| format!("read {}: {error}", scope.display()))?;
    for entry in entries {
        let entry =
            entry.map_err(|error| format!("read entry under {}: {error}", scope.display()))?;
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .map_err(|error| format!("relative profile path {}: {error}", path.display()))?
            .to_path_buf();
        let file_type = entry
            .file_type()
            .map_err(|error| format!("read file type {}: {error}", path.display()))?;
        let value = if file_type.is_symlink() {
            IdentityEntry::Symlink(
                fs::read_link(&path)
                    .map_err(|error| format!("read symlink {}: {error}", path.display()))?,
            )
        } else if file_type.is_file() {
            IdentityEntry::File(
                fs::read(&path).map_err(|error| format!("read {}: {error}", path.display()))?,
            )
        } else if file_type.is_dir() {
            directory_identity(&path)?
        } else {
            return Err(format!(
                "unsupported scoped package entry type: {}",
                path.display()
            ));
        };
        identity.insert(relative, value);
    }
    Ok(())
}

fn directory_identity(_path: &Path) -> Result<IdentityEntry, String> {
    Ok(IdentityEntry::Directory)
}

fn bytes_fingerprint(bytes: &[u8]) -> String {
    let mut digest = Sha256::new();
    hash_field(&mut digest, b"file", bytes);
    format!("{:x}", digest.finalize())
}

fn identity_fingerprint(identity: &ProfileIdentity) -> String {
    let mut digest = Sha256::new();
    digest.update(b"dsh-desktop-profile-identity-v1\0");
    for (path, entry) in identity {
        hash_os_str(&mut digest, path.as_os_str());
        match entry {
            IdentityEntry::File(bytes) => {
                hash_field(&mut digest, b"file", bytes);
            }
            IdentityEntry::Directory => {
                hash_field(&mut digest, b"directory", &[]);
            }
            IdentityEntry::Symlink(target) => {
                digest.update(b"symlink\0");
                hash_os_str(&mut digest, target.as_os_str());
            }
        }
    }
    format!("{:x}", digest.finalize())
}

fn hash_field(digest: &mut Sha256, kind: &[u8], bytes: &[u8]) {
    digest.update((kind.len() as u64).to_le_bytes());
    digest.update(kind);
    digest.update((bytes.len() as u64).to_le_bytes());
    digest.update(bytes);
}

#[cfg(unix)]
fn hash_os_str(digest: &mut Sha256, value: &OsStr) {
    use std::os::unix::ffi::OsStrExt;

    hash_field(digest, b"os-unix", value.as_bytes());
}

#[cfg(windows)]
fn hash_os_str(digest: &mut Sha256, value: &OsStr) {
    use std::os::windows::ffi::OsStrExt;

    let bytes = value
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    hash_field(digest, b"os-windows-utf16le", &bytes);
}

fn copy_profile_tree(source: &Path, target: &Path) -> Result<(), String> {
    fs::create_dir_all(target).map_err(|error| format!("create {}: {error}", target.display()))?;
    let entries =
        fs::read_dir(source).map_err(|error| format!("read {}: {error}", source.display()))?;
    for entry in entries {
        let entry =
            entry.map_err(|error| format!("read entry under {}: {error}", source.display()))?;
        if source
            .file_name()
            .is_some_and(|name| name == OsStr::new(PROFILE_NAME))
            && entry.file_name() == OsStr::new("node_modules")
        {
            continue;
        }
        let from = entry.path();
        let to = target.join(entry.file_name());
        let file_type = entry
            .file_type()
            .map_err(|error| format!("read file type {}: {error}", from.display()))?;
        if file_type.is_dir() {
            copy_profile_tree(&from, &to)?;
        } else if file_type.is_file() {
            fs::copy(&from, &to)
                .map_err(|error| format!("copy {} to {}: {error}", from.display(), to.display()))?;
        } else if file_type.is_symlink() {
            copy_symlink(&from, &to)?;
        } else {
            return Err(format!(
                "unsupported profile entry type: {}",
                from.display()
            ));
        }
    }
    Ok(())
}

fn copy_symlink(source: &Path, target: &Path) -> Result<(), String> {
    let link = fs::read_link(source)
        .map_err(|error| format!("read symlink {}: {error}", source.display()))?;
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&link, target).map_err(|error| {
            format!(
                "copy symlink {} to {}: {error}",
                source.display(),
                target.display()
            )
        })?;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileTypeExt;

        let file_type = fs::symlink_metadata(source)
            .map_err(|error| format!("read symlink type {}: {error}", source.display()))?
            .file_type();
        let result = if file_type.is_symlink_dir() {
            std::os::windows::fs::symlink_dir(&link, target)
        } else if file_type.is_symlink_file() {
            std::os::windows::fs::symlink_file(&link, target)
        } else {
            return Err(format!(
                "unsupported Windows reparse point: {}",
                source.display()
            ));
        };
        result.map_err(|error| {
            format!(
                "copy symlink {} to {}: {error}",
                source.display(),
                target.display()
            )
        })?;
    }
    Ok(())
}

fn rename_with_retry(source: &Path, target: &Path) -> std::io::Result<()> {
    let mut last_error = None;
    for attempt in 0..RENAME_ATTEMPTS {
        match fs::rename(source, target) {
            Ok(()) => return Ok(()),
            Err(error) => {
                last_error = Some(error);
                if attempt + 1 < RENAME_ATTEMPTS {
                    thread::sleep(RENAME_RETRY_DELAY);
                }
            }
        }
    }
    Err(last_error.expect("at least one rename attempt"))
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), String> {
    fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("sync directory {}: {error}", path.display()))
}

#[cfg(windows)]
fn sync_directory(_path: &Path) -> Result<(), String> {
    Ok(())
}

fn remove_path_if_exists(path: &Path) -> std::io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => remove_path(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn remove_path(path: &Path) -> std::io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_home(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "dsh-desktop-profile-repair-{}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root.join("home")
    }

    fn seed_profile(home: &Path) {
        let profile = home.join("profiles/web");
        fs::create_dir_all(profile.join("node_modules")).unwrap();
        fs::create_dir_all(profile.join("custom/nested")).unwrap();
        fs::write(profile.join("package.json"), "{\"name\":\"original\"}\n").unwrap();
        fs::write(profile.join("pnpm-lock.yaml"), "original-lock\n").unwrap();
        fs::write(profile.join("pnpm-workspace.yaml"), "packages:\n  - .\n").unwrap();
        fs::write(profile.join("cordis.patch.yml"), "[]\n").unwrap();
        fs::write(profile.join("node_modules/original"), "keep\n").unwrap();
        fs::write(profile.join("custom/nested/value"), "preserved\n").unwrap();
    }

    fn complete_shadow(shadow_home: &Path, name: &str) {
        let profile = shadow_home.join("profiles/web");
        fs::create_dir_all(profile.join("node_modules")).unwrap();
        fs::write(
            profile.join("package.json"),
            format!("{{\"name\":\"{name}\"}}\n"),
        )
        .unwrap();
        for (file, content) in [
            ("pnpm-workspace.yaml", "packages:\n  - .\n"),
            ("cordis.patch.yml", "[]\n"),
        ] {
            if !profile.join(file).exists() {
                fs::write(profile.join(file), content).unwrap();
            }
        }
    }

    fn cleanup(home: &Path) {
        if let Some(root) = home.parent() {
            let _ = fs::remove_dir_all(root);
        }
    }

    #[test]
    fn commits_a_complete_staged_profile() {
        let home = scratch_home("commit");
        seed_profile(&home);
        fs::write(home.join("cordis.patch.yml"), "- id: home-layer\n").unwrap();
        mutate_web_profile(&home, &[], |shadow_home, had_original| {
            assert!(had_original);
            let profile = shadow_home.join("profiles/web");
            assert!(!profile.join("node_modules").exists());
            assert_eq!(
                fs::read_to_string(profile.join("custom/nested/value")).unwrap(),
                "preserved\n"
            );
            assert_eq!(
                fs::read_to_string(shadow_home.join("cordis.patch.yml")).unwrap(),
                "- id: home-layer\n"
            );
            complete_shadow(shadow_home, "repaired");
            fs::write(profile.join("node_modules/repaired"), "ready\n").unwrap();
            Ok(())
        })
        .unwrap();

        let profile = home.join("profiles/web");
        assert!(fs::read_to_string(profile.join("package.json"))
            .unwrap()
            .contains("repaired"));
        assert!(profile.join("node_modules/repaired").is_file());
        assert!(!profile.join("node_modules/original").exists());
        assert_eq!(
            fs::read_to_string(home.join("cordis.patch.yml")).unwrap(),
            "- id: home-layer\n"
        );
        assert!(!home.join(JOURNAL_NAME).exists());
        cleanup(&home);
    }

    #[test]
    fn validates_managed_manifest_and_link_targets() {
        let home = scratch_home("managed-target");
        let source = home.parent().unwrap().join("desktop-owned-source");
        fs::create_dir_all(&source).unwrap();
        fs::write(
            source.join("package.json"),
            "{\"name\":\"desktop-owned\"}\n",
        )
        .unwrap();
        let targets = [("desktop-owned", source.as_path())];
        mutate_web_profile(&home, &targets, |shadow_home, _| {
            complete_shadow(shadow_home, "managed-target");
            let profile = shadow_home.join("profiles/web");
            fs::write(
                profile.join("package.json"),
                "{\"name\":\"managed-target\",\"dependencies\":{\"desktop-owned\":\"link:fixture\"}}\n",
            )
            .map_err(|error| format!("write managed target fixture: {error}"))?;
            super::super::link_dir(&source, &profile.join("node_modules/desktop-owned"))
        })
        .unwrap();

        assert_eq!(
            fs::canonicalize(home.join("profiles/web/node_modules/desktop-owned")).unwrap(),
            fs::canonicalize(&source).unwrap()
        );
        cleanup(&home);
    }

    #[test]
    fn failed_mutation_leaves_the_real_profile_untouched() {
        let home = scratch_home("rollback");
        seed_profile(&home);
        let error = mutate_web_profile(&home, &[], |shadow_home, _| {
            complete_shadow(shadow_home, "partial");
            Err("simulated install failure".to_string())
        })
        .unwrap_err();

        assert!(error.contains("simulated install failure"));
        let profile = home.join("profiles/web");
        assert!(fs::read_to_string(profile.join("package.json"))
            .unwrap()
            .contains("original"));
        assert!(profile.join("node_modules/original").is_file());
        assert!(!home.join(JOURNAL_NAME).exists());
        cleanup(&home);
    }

    #[test]
    fn external_manifest_change_aborts_the_commit() {
        let home = scratch_home("compare-and-swap");
        seed_profile(&home);
        let real = home.join("profiles/web/package.json");
        let error = mutate_web_profile(&home, &[], |shadow_home, _| {
            complete_shadow(shadow_home, "candidate");
            fs::write(&real, "{\"name\":\"terminal-change\"}\n").unwrap();
            Ok(())
        })
        .unwrap_err();
        assert!(error.contains("changed outside"));
        assert!(fs::read_to_string(real)
            .unwrap()
            .contains("terminal-change"));
        cleanup(&home);
    }

    #[test]
    fn external_home_patch_change_aborts_the_commit() {
        let home = scratch_home("home-patch-compare-and-swap");
        seed_profile(&home);
        let patch = home.join("cordis.patch.yml");
        fs::write(&patch, "- id: original-home-layer\n").unwrap();
        let error = mutate_web_profile(&home, &[], |shadow_home, _| {
            complete_shadow(shadow_home, "candidate");
            fs::write(&patch, "- id: terminal-change\n").unwrap();
            Ok(())
        })
        .unwrap_err();
        assert!(error.contains("home cordis.patch.yml changed outside"));
        assert!(fs::read_to_string(home.join("profiles/web/package.json"))
            .unwrap()
            .contains("original"));
        assert_eq!(
            fs::read_to_string(patch).unwrap(),
            "- id: terminal-change\n"
        );
        cleanup(&home);
    }

    #[test]
    fn external_node_modules_change_aborts_the_commit() {
        let home = scratch_home("node-modules-compare-and-swap");
        seed_profile(&home);
        let real_entry = home.join("profiles/web/node_modules/terminal-change");
        let error = mutate_web_profile(&home, &[], |shadow_home, _| {
            complete_shadow(shadow_home, "candidate");
            fs::write(&real_entry, "new dependency state\n").unwrap();
            Ok(())
        })
        .unwrap_err();
        assert!(error.contains("changed outside"));
        assert!(real_entry.is_file());
        cleanup(&home);
    }

    #[test]
    fn initializes_a_missing_profile_in_the_shadow_home() {
        let home = scratch_home("new");
        mutate_web_profile(&home, &[], |shadow_home, had_original| {
            assert!(!had_original);
            complete_shadow(shadow_home, "new");
            Ok(())
        })
        .unwrap();
        assert!(home.join("profiles/web/package.json").is_file());
        cleanup(&home);
    }

    #[test]
    fn stale_journal_restores_the_backup_before_new_work() {
        let home = scratch_home("recovery");
        fs::create_dir_all(&home).unwrap();
        let id = "999-1";
        let paths = repair_paths(&home, id).unwrap();
        fs::create_dir_all(&paths.backup).unwrap();
        fs::write(
            paths.backup.join("package.json"),
            "{\"name\":\"original\"}\n",
        )
        .unwrap();
        let original_identity = Some(identity_fingerprint(
            &capture_profile_identity(&paths.backup).unwrap(),
        ));
        complete_shadow(&paths.shadow_home, "partial");
        let journal = RepairJournal {
            schema: JOURNAL_SCHEMA,
            owner_pid: u32::MAX,
            owner_lstart: "stale".into(),
            created_unix_ms: 1,
            id: id.into(),
            phase: RepairPhase::OriginalMoved,
            had_original: true,
            real_profile: paths.profile.clone(),
            shadow_profile: paths.shadow_profile.clone(),
            backup_profile: paths.backup.clone(),
            targets: Vec::new(),
            original_identity,
            home_patch_identity: None,
        };
        write_new_journal(&paths.journal, &journal).unwrap();

        recover_stale_repair(&home).unwrap();
        assert!(fs::read_to_string(paths.profile.join("package.json"))
            .unwrap()
            .contains("original"));
        assert!(!paths.backup.exists());
        assert!(!paths.shadow_home.exists());
        assert!(!paths.journal.exists());
        cleanup(&home);
    }

    #[test]
    fn rolling_back_phase_restores_original_after_candidate_move() {
        let home = scratch_home("rolling-back");
        fs::create_dir_all(&home).unwrap();
        let id = "999-6";
        let paths = repair_paths(&home, id).unwrap();
        fs::create_dir_all(&paths.backup).unwrap();
        fs::write(
            paths.backup.join("package.json"),
            "{\"name\":\"original\"}\n",
        )
        .unwrap();
        let original_identity = Some(identity_fingerprint(
            &capture_profile_identity(&paths.backup).unwrap(),
        ));
        complete_shadow(&paths.shadow_home, "candidate");
        let journal = RepairJournal {
            schema: JOURNAL_SCHEMA,
            owner_pid: u32::MAX,
            owner_lstart: "stale".into(),
            created_unix_ms: 1,
            id: id.into(),
            phase: RepairPhase::RollingBack,
            had_original: true,
            real_profile: paths.profile.clone(),
            shadow_profile: paths.shadow_profile.clone(),
            backup_profile: paths.backup.clone(),
            targets: Vec::new(),
            original_identity,
            home_patch_identity: None,
        };
        write_new_journal(&paths.journal, &journal).unwrap();

        recover_stale_repair(&home).unwrap();
        assert!(fs::read_to_string(paths.profile.join("package.json"))
            .unwrap()
            .contains("original"));
        assert!(!paths.shadow_home.exists());
        assert!(!paths.journal.exists());
        cleanup(&home);
    }

    #[test]
    fn promoted_marker_finishes_an_interrupted_commit() {
        let home = scratch_home("promoted");
        fs::create_dir_all(&home).unwrap();
        let id = "999-2";
        let paths = repair_paths(&home, id).unwrap();
        complete_shadow(&paths.shadow_home, "candidate");
        fs::write(paths.shadow_profile.join(MARKER_NAME), format!("{id}\n")).unwrap();
        fs::create_dir_all(paths.profile.parent().unwrap()).unwrap();
        fs::rename(&paths.shadow_profile, &paths.profile).unwrap();
        fs::create_dir_all(&paths.backup).unwrap();
        fs::write(
            paths.backup.join("package.json"),
            "{\"name\":\"original\"}\n",
        )
        .unwrap();
        let original_identity = Some(identity_fingerprint(
            &capture_profile_identity(&paths.backup).unwrap(),
        ));
        let journal = RepairJournal {
            schema: JOURNAL_SCHEMA,
            owner_pid: u32::MAX,
            owner_lstart: "stale".into(),
            created_unix_ms: 1,
            id: id.into(),
            phase: RepairPhase::OriginalMoved,
            had_original: true,
            real_profile: paths.profile.clone(),
            shadow_profile: paths.shadow_profile.clone(),
            backup_profile: paths.backup.clone(),
            targets: Vec::new(),
            original_identity,
            home_patch_identity: None,
        };
        write_new_journal(&paths.journal, &journal).unwrap();

        recover_stale_repair(&home).unwrap();
        assert!(fs::read_to_string(paths.profile.join("package.json"))
            .unwrap()
            .contains("candidate"));
        assert!(!paths.profile.join(MARKER_NAME).exists());
        assert!(!paths.backup.exists());
        assert!(!paths.journal.exists());
        cleanup(&home);
    }

    #[test]
    fn changed_home_patch_rolls_back_an_interrupted_promotion() {
        let home = scratch_home("promoted-home-patch-change");
        seed_profile(&home);
        let patch = home.join("cordis.patch.yml");
        let original_patch = b"- id: original-home-layer\n";
        fs::write(&patch, original_patch).unwrap();
        let id = "999-7";
        let paths = repair_paths(&home, id).unwrap();
        let original_identity = Some(identity_fingerprint(
            &capture_profile_identity(&paths.profile).unwrap(),
        ));
        fs::rename(&paths.profile, &paths.backup).unwrap();
        complete_shadow(&paths.shadow_home, "candidate");
        fs::write(paths.shadow_profile.join(MARKER_NAME), format!("{id}\n")).unwrap();
        fs::rename(&paths.shadow_profile, &paths.profile).unwrap();
        let journal = RepairJournal {
            schema: JOURNAL_SCHEMA,
            owner_pid: u32::MAX,
            owner_lstart: "stale".into(),
            created_unix_ms: 1,
            id: id.into(),
            phase: RepairPhase::ShadowPromoted,
            had_original: true,
            real_profile: paths.profile.clone(),
            shadow_profile: paths.shadow_profile.clone(),
            backup_profile: paths.backup.clone(),
            targets: Vec::new(),
            original_identity,
            home_patch_identity: Some(bytes_fingerprint(original_patch)),
        };
        write_new_journal(&paths.journal, &journal).unwrap();
        fs::write(&patch, "- id: terminal-change\n").unwrap();

        recover_stale_repair(&home).unwrap();
        assert!(fs::read_to_string(paths.profile.join("package.json"))
            .unwrap()
            .contains("original"));
        assert_eq!(
            fs::read_to_string(patch).unwrap(),
            "- id: terminal-change\n"
        );
        assert!(!paths.backup.exists());
        assert!(!paths.journal.exists());
        cleanup(&home);
    }

    #[test]
    fn marker_without_journal_fails_loud() {
        let home = scratch_home("orphan-marker");
        seed_profile(&home);
        let marker = home.join("profiles/web").join(MARKER_NAME);
        fs::write(&marker, "999-4\n").unwrap();
        assert!(recover_stale_repair(&home)
            .unwrap_err()
            .contains("without a journal"));
        assert!(marker.is_file());
        cleanup(&home);
    }

    #[test]
    fn phase_and_marker_conflict_preserves_every_path() {
        let home = scratch_home("phase-conflict");
        fs::create_dir_all(&home).unwrap();
        let id = "999-5";
        let paths = repair_paths(&home, id).unwrap();
        complete_shadow(&paths.shadow_home, "candidate");
        fs::write(paths.shadow_profile.join(MARKER_NAME), format!("{id}\n")).unwrap();
        fs::create_dir_all(paths.profile.parent().unwrap()).unwrap();
        fs::rename(&paths.shadow_profile, &paths.profile).unwrap();
        let journal = RepairJournal {
            schema: JOURNAL_SCHEMA,
            owner_pid: u32::MAX,
            owner_lstart: "stale".into(),
            created_unix_ms: 1,
            id: id.into(),
            phase: RepairPhase::Prepared,
            had_original: false,
            real_profile: paths.profile.clone(),
            shadow_profile: paths.shadow_profile.clone(),
            backup_profile: paths.backup.clone(),
            targets: Vec::new(),
            original_identity: None,
            home_patch_identity: None,
        };
        write_new_journal(&paths.journal, &journal).unwrap();

        assert!(recover_stale_repair(&home)
            .unwrap_err()
            .contains("ambiguous"));
        assert!(paths.profile.is_dir());
        assert!(paths.journal.is_file());
        cleanup(&home);
    }

    #[test]
    fn phase_updates_keep_the_primary_journal_parseable() {
        let home = scratch_home("append-only-phase");
        fs::create_dir_all(&home).unwrap();
        let id = "999-6";
        let paths = repair_paths(&home, id).unwrap();
        let mut journal = RepairJournal {
            schema: JOURNAL_SCHEMA,
            owner_pid: u32::MAX,
            owner_lstart: "stale".into(),
            created_unix_ms: 1,
            id: id.into(),
            phase: RepairPhase::Prepared,
            had_original: false,
            real_profile: paths.profile.clone(),
            shadow_profile: paths.shadow_profile.clone(),
            backup_profile: paths.backup.clone(),
            targets: Vec::new(),
            original_identity: None,
            home_patch_identity: None,
        };
        write_new_journal(&paths.journal, &journal).unwrap();
        let primary = fs::read(&paths.journal).unwrap();

        journal.phase = RepairPhase::OriginalMoved;
        update_journal(&paths.journal, &journal).unwrap();
        let unpublished = phase_record_path(&paths.journal, id, RepairPhase::ShadowPromoted)
            .unwrap()
            .with_extension(format!("phase.{}.tmp", std::process::id()));
        fs::write(unpublished, b"partial phase record").unwrap();

        assert_eq!(fs::read(&paths.journal).unwrap(), primary);
        let parsed: RepairJournal =
            serde_json::from_slice(&fs::read(&paths.journal).unwrap()).unwrap();
        assert_eq!(
            read_durable_phase(&paths.journal, &parsed).unwrap(),
            RepairPhase::OriginalMoved
        );
        remove_journal_records(&paths.journal).unwrap();
        cleanup(&home);
    }

    #[test]
    fn live_journal_blocks_a_second_mutation() {
        let home = scratch_home("live-owner");
        fs::create_dir_all(&home).unwrap();
        let pid = std::process::id();
        let id = "999-3";
        let paths = repair_paths(&home, id).unwrap();
        let journal = RepairJournal {
            schema: JOURNAL_SCHEMA,
            owner_pid: pid,
            owner_lstart: super::super::ps_lstart(pid).expect("test process has a start token"),
            created_unix_ms: 1,
            id: id.into(),
            phase: RepairPhase::Prepared,
            had_original: false,
            real_profile: paths.profile.clone(),
            shadow_profile: paths.shadow_profile.clone(),
            backup_profile: paths.backup.clone(),
            targets: Vec::new(),
            original_identity: None,
            home_patch_identity: None,
        };
        write_new_journal(&paths.journal, &journal).unwrap();
        assert!(recover_stale_repair(&home)
            .unwrap_err()
            .contains("already owned"));
        fs::remove_file(paths.journal).unwrap();
        cleanup(&home);
    }
}
