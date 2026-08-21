//! Consent and persistent configuration snapshots for a shared DSH_HOME.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const RECORD_SCHEMA: u8 = 1;
const BACKUP_SCHEMA: u8 = 1;
const CONSENT_SCOPE: &str = "desktop-owned-web-profile-v1";
const RECORDS_DIR: &str = "profile-adoptions";
const BACKUPS_DIR: &str = "profile-backups";
const RECORD_LOCK: &str = ".append.lock";
const RECORD_LOCK_WAIT: Duration = Duration::from_secs(2);
const RECORD_LOCK_STALE: Duration = Duration::from_secs(30);

static FILE_NONCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) enum AdoptionOrigin {
    FreshHome,
    ExistingHome,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) enum AdoptionStatus {
    Adopting,
    Active,
    ConsentRequired,
    RestorePending,
    Restored,
    RestoreAbandoned,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct BackupRef {
    pub id: String,
    pub root: PathBuf,
    pub profile: PathBuf,
    pub source_identity: String,
    pub snapshot_identity: String,
    pub created_unix_ms: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AdoptionRecord {
    schema: u8,
    scope: String,
    pub revision: u64,
    pub dsh_home: PathBuf,
    pub origin: AdoptionOrigin,
    pub status: AdoptionStatus,
    pub consented_unix_ms: Option<u64>,
    pub updated_unix_ms: u64,
    pub backup: Option<BackupRef>,
    pub restore_source_identity: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct BackupManifest {
    schema: u8,
    scope: String,
    id: String,
    dsh_home: PathBuf,
    source_identity: String,
    snapshot_identity: String,
    created_unix_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ExistingHomeSummary {
    pub canonical_home: PathBuf,
    pub has_existing_data: bool,
    pub has_web_profile: bool,
    pub plugins: Vec<String>,
    pub agent_preset_count: usize,
}

pub(super) fn inspect_home(dsh_home: &Path) -> Result<ExistingHomeSummary, String> {
    let canonical_home = canonical_home(dsh_home)?;
    if !dsh_home.exists() {
        return Ok(ExistingHomeSummary {
            canonical_home,
            has_existing_data: false,
            has_web_profile: false,
            plugins: Vec::new(),
            agent_preset_count: 0,
        });
    }
    if !dsh_home.is_dir() {
        return Err(format!(
            "DSH_HOME is not a directory: {}",
            dsh_home.display()
        ));
    }

    let profile = dsh_home.join("profiles/web");
    let has_web_profile = profile.is_dir();
    let plugins = read_profile_plugins(&profile)?;
    let agent_preset_count = count_agent_presets(&dsh_home.join(".agent-presets"))?;
    let has_existing_data = has_web_profile
        || agent_preset_count > 0
        || fs::read_dir(dsh_home)
            .map_err(|error| format!("read DSH_HOME {}: {error}", dsh_home.display()))?
            .filter_map(Result::ok)
            .any(|entry| is_meaningful_home_entry(&entry.file_name()));

    Ok(ExistingHomeSummary {
        canonical_home,
        has_existing_data,
        has_web_profile,
        plugins,
        agent_preset_count,
    })
}

pub(super) fn latest_record(
    shell_root: &Path,
    canonical_home: &Path,
) -> Result<Option<AdoptionRecord>, String> {
    let key = home_key(canonical_home);
    let dir = shell_root.join(RECORDS_DIR).join(&key);
    if !dir.exists() {
        return Ok(None);
    }
    let mut records = Vec::new();
    let mut invalid_seen = false;
    for entry in fs::read_dir(&dir).map_err(|error| format!("read {}: {error}", dir.display()))? {
        let entry =
            entry.map_err(|error| format!("read entry under {}: {error}", dir.display()))?;
        let path = entry.path();
        if path.extension() != Some(OsStr::new("json")) {
            continue;
        }
        let record = fs::read(&path)
            .map_err(|error| format!("read adoption record: {error}"))
            .and_then(|bytes| {
                serde_json::from_slice::<AdoptionRecord>(&bytes)
                    .map_err(|error| format!("parse adoption record: {error}"))
            })
            .and_then(|record| {
                validate_record(&record, canonical_home)?;
                Ok(record)
            });
        match record {
            Ok(record) => records.push((entry.file_name(), record)),
            Err(error) => {
                invalid_seen = true;
                let quarantine_error = quarantine_invalid_record(&path).err();
                eprintln!(
                    "dsh-desktop: preserving and quarantining invalid adoption record {}: {error}{}",
                    path.display(),
                    quarantine_error
                        .as_deref()
                        .map(|error| format!("; quarantine failed: {error}"))
                        .unwrap_or_default()
                );
            }
        }
    }
    let Some(max_revision) = records.iter().map(|(_, record)| record.revision).max() else {
        return Ok(None);
    };
    let mut latest = records
        .into_iter()
        .filter(|(_, record)| record.revision == max_revision)
        .collect::<Vec<_>>();
    latest.sort_by(|(left, _), (right, _)| left.cmp(right));
    let mut selected = latest
        .pop()
        .map(|(_, record)| record)
        .ok_or_else(|| "adoption record selection unexpectedly became empty".to_string())?;
    if invalid_seen || !latest.is_empty() {
        eprintln!(
            "dsh-desktop: ambiguous adoption history at revision {max_revision} for {}; requiring fresh consent",
            canonical_home.display()
        );
        selected.status = AdoptionStatus::ConsentRequired;
        selected.backup = None;
        selected.restore_source_identity = None;
    }
    Ok(Some(selected))
}

fn quarantine_invalid_record(path: &Path) -> Result<(), String> {
    let nonce = FILE_NONCE.fetch_add(1, Ordering::Relaxed);
    let file_name = path
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or("adoption-record.json");
    let quarantine = path.with_file_name(format!(
        "{file_name}.invalid-{}-{nonce}",
        std::process::id()
    ));
    match fs::rename(path, &quarantine) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "rename {} to {}: {error}",
            path.display(),
            quarantine.display()
        )),
    }
}

pub(super) fn start_record(
    shell_root: &Path,
    canonical_home: &Path,
    origin: AdoptionOrigin,
    consented: bool,
    backup: Option<BackupRef>,
) -> Result<AdoptionRecord, String> {
    if latest_record(shell_root, canonical_home)?.is_some() {
        return Err(format!(
            "adoption state already exists for {}",
            canonical_home.display()
        ));
    }
    let now = unix_millis()?;
    let record = AdoptionRecord {
        schema: RECORD_SCHEMA,
        scope: CONSENT_SCOPE.to_string(),
        revision: 1,
        dsh_home: canonical_home.to_path_buf(),
        origin,
        status: AdoptionStatus::Adopting,
        consented_unix_ms: consented.then_some(now),
        updated_unix_ms: now,
        backup,
        restore_source_identity: None,
    };
    append_record(shell_root, None, &record)?;
    Ok(record)
}

pub(super) fn restart_with_consent(
    shell_root: &Path,
    previous: &AdoptionRecord,
    backup: Option<BackupRef>,
) -> Result<AdoptionRecord, String> {
    if !matches!(
        previous.status,
        AdoptionStatus::ConsentRequired
            | AdoptionStatus::Restored
            | AdoptionStatus::RestoreAbandoned
    ) {
        return Err("current adoption state cannot request consent again".to_string());
    }
    let now = unix_millis()?;
    let mut record = previous.clone();
    record.revision = previous
        .revision
        .checked_add(1)
        .ok_or_else(|| "adoption revision overflow".to_string())?;
    record.origin = AdoptionOrigin::ExistingHome;
    record.status = AdoptionStatus::Adopting;
    record.consented_unix_ms = Some(now);
    record.updated_unix_ms = now;
    record.backup = backup;
    record.restore_source_identity = None;
    append_record(shell_root, Some(previous), &record)?;
    Ok(record)
}

pub(super) fn begin_restore(
    shell_root: &Path,
    previous: &AdoptionRecord,
    source_identity: String,
) -> Result<AdoptionRecord, String> {
    if previous.backup.is_none() {
        return Err("cannot restore without a verified profile backup".to_string());
    }
    let mut record = previous.clone();
    record.revision = previous
        .revision
        .checked_add(1)
        .ok_or_else(|| "adoption revision overflow".to_string())?;
    record.status = AdoptionStatus::RestorePending;
    record.updated_unix_ms = unix_millis()?;
    record.restore_source_identity = Some(source_identity);
    append_record(shell_root, Some(previous), &record)?;
    Ok(record)
}

pub(super) fn transition(
    shell_root: &Path,
    previous: &AdoptionRecord,
    status: AdoptionStatus,
    backup: Option<BackupRef>,
) -> Result<AdoptionRecord, String> {
    let mut record = previous.clone();
    record.revision = previous
        .revision
        .checked_add(1)
        .ok_or_else(|| "adoption revision overflow".to_string())?;
    record.status = status;
    record.updated_unix_ms = unix_millis()?;
    record.backup = backup;
    if status != AdoptionStatus::Restored {
        record.restore_source_identity = None;
    }
    append_record(shell_root, Some(previous), &record)?;
    Ok(record)
}

pub(super) fn create_backup(shell_root: &Path, canonical_home: &Path) -> Result<BackupRef, String> {
    let source = canonical_home.join("profiles/web");
    if !source.is_dir() {
        return Err(format!(
            "cannot back up missing web profile: {}",
            source.display()
        ));
    }
    let source_before = super::profile_repair::web_profile_identity(canonical_home)?
        .ok_or_else(|| "web profile disappeared before backup".to_string())?;
    let created_unix_ms = unix_millis()?;
    let id = format!(
        "{}-{created_unix_ms}-{}",
        std::process::id(),
        FILE_NONCE.fetch_add(1, Ordering::Relaxed)
    );
    validate_backup_id(&id)?;
    let parent = backup_parent(shell_root, canonical_home);
    fs::create_dir_all(&parent)
        .map_err(|error| format!("create backup directory {}: {error}", parent.display()))?;
    let root = parent.join(&id);
    let temp = parent.join(format!(".{id}.tmp"));
    if root.exists() || temp.exists() {
        return Err(format!("profile backup id already exists: {id}"));
    }
    let temp_profile = temp.join("web");

    let result = (|| {
        fs::create_dir_all(&temp)
            .map_err(|error| format!("create backup staging {}: {error}", temp.display()))?;
        super::profile_repair::copy_profile_snapshot(&source, &temp_profile)?;
        let snapshot_identity = super::profile_repair::profile_snapshot_identity(&temp_profile)?;
        let source_after = super::profile_repair::web_profile_identity(canonical_home)?
            .ok_or_else(|| "web profile disappeared while backing it up".to_string())?;
        if source_after != source_before {
            return Err("web profile changed while creating the approved backup".to_string());
        }
        let manifest = BackupManifest {
            schema: BACKUP_SCHEMA,
            scope: CONSENT_SCOPE.to_string(),
            id: id.clone(),
            dsh_home: canonical_home.to_path_buf(),
            source_identity: source_before.clone(),
            snapshot_identity: snapshot_identity.clone(),
            created_unix_ms,
        };
        let bytes = serde_json::to_vec_pretty(&manifest)
            .map_err(|error| format!("serialize profile backup manifest: {error}"))?;
        write_synced(&temp.join("manifest.json"), &bytes)?;
        let checksum = bytes_fingerprint(&bytes);
        write_synced(&temp.join(".ok"), format!("{checksum}\n").as_bytes())?;
        sync_tree(&temp)?;
        fs::rename(&temp, &root).map_err(|error| {
            format!(
                "publish profile backup {} as {}: {error}",
                temp.display(),
                root.display()
            )
        })?;
        sync_directory(&parent)?;
        Ok(BackupRef {
            id: id.clone(),
            root: root.clone(),
            profile: root.join("web"),
            source_identity: source_before.clone(),
            snapshot_identity,
            created_unix_ms,
        })
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&temp);
    }
    result
}

pub(super) fn verify_backup(
    shell_root: &Path,
    canonical_home: &Path,
    backup: &BackupRef,
) -> Result<(), String> {
    validate_backup_id(&backup.id)?;
    let expected_root = backup_parent(shell_root, canonical_home).join(&backup.id);
    if backup.root != expected_root || backup.profile != expected_root.join("web") {
        return Err("profile backup path escapes the shell backup root".to_string());
    }
    let manifest_path = backup.root.join("manifest.json");
    let bytes = fs::read(&manifest_path).map_err(|error| {
        format!(
            "read profile backup manifest {}: {error}",
            manifest_path.display()
        )
    })?;
    let ok = fs::read_to_string(backup.root.join(".ok"))
        .map_err(|error| format!("read profile backup completion marker: {error}"))?;
    if ok.trim() != bytes_fingerprint(&bytes) {
        return Err("profile backup completion checksum does not match its manifest".to_string());
    }
    let manifest: BackupManifest = serde_json::from_slice(&bytes)
        .map_err(|error| format!("parse profile backup manifest: {error}"))?;
    if manifest.schema != BACKUP_SCHEMA
        || manifest.scope != CONSENT_SCOPE
        || manifest.id != backup.id
        || manifest.dsh_home != canonical_home
        || manifest.source_identity != backup.source_identity
        || manifest.snapshot_identity != backup.snapshot_identity
        || manifest.created_unix_ms != backup.created_unix_ms
    {
        return Err("profile backup manifest does not match its adoption record".to_string());
    }
    let actual = super::profile_repair::profile_snapshot_identity(&backup.profile)?;
    if actual != backup.snapshot_identity {
        return Err("profile backup contents no longer match their manifest".to_string());
    }
    Ok(())
}

pub(super) fn current_profile_matches_backup(
    canonical_home: &Path,
    backup: &BackupRef,
) -> Result<bool, String> {
    let profile = canonical_home.join("profiles/web");
    if !profile.is_dir() {
        return Ok(false);
    }
    Ok(super::profile_repair::profile_snapshot_identity(&profile)? == backup.snapshot_identity)
}

pub(super) fn cleanup_stale_backup_staging(
    shell_root: &Path,
    canonical_home: &Path,
) -> Result<(), String> {
    let parent = backup_parent(shell_root, canonical_home);
    if !parent.is_dir() {
        return Ok(());
    }
    let now = SystemTime::now();
    for entry in
        fs::read_dir(&parent).map_err(|error| format!("read {}: {error}", parent.display()))?
    {
        let entry =
            entry.map_err(|error| format!("read entry under {}: {error}", parent.display()))?;
        let path = entry.path();
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !path.is_dir() || !name.starts_with('.') || !name.ends_with(".tmp") {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .map_err(|error| format!("inspect backup staging {}: {error}", path.display()))?;
        let stale = now
            .duration_since(modified)
            .map(|age| age.as_secs() >= 24 * 60 * 60)
            .unwrap_or(false);
        if stale {
            fs::remove_dir_all(&path).map_err(|error| {
                format!("remove stale backup staging {}: {error}", path.display())
            })?;
        }
    }
    sync_directory(&parent)
}

pub(super) fn prune_other_backups(
    shell_root: &Path,
    canonical_home: &Path,
    keep: &BackupRef,
) -> Result<(), String> {
    let parent = backup_parent(shell_root, canonical_home);
    if !parent.is_dir() {
        return Ok(());
    }
    for entry in
        fs::read_dir(&parent).map_err(|error| format!("read {}: {error}", parent.display()))?
    {
        let entry =
            entry.map_err(|error| format!("read entry under {}: {error}", parent.display()))?;
        if entry.file_name() == OsStr::new(keep.id.as_str()) {
            continue;
        }
        let path = entry.path();
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if path.is_dir() && validate_backup_id(&name).is_ok() {
            if backup_root_is_fully_valid(shell_root, canonical_home, &path) {
                fs::remove_dir_all(&path).map_err(|error| {
                    format!("remove superseded backup {}: {error}", path.display())
                })?;
            } else {
                eprintln!(
                    "dsh-desktop: preserving unverifiable superseded backup {}",
                    path.display()
                );
            }
        }
    }
    sync_directory(&parent)
}

fn backup_root_is_fully_valid(shell_root: &Path, canonical_home: &Path, root: &Path) -> bool {
    let Ok(bytes) = fs::read(root.join("manifest.json")) else {
        return false;
    };
    let Ok(manifest) = serde_json::from_slice::<BackupManifest>(&bytes) else {
        return false;
    };
    let backup = BackupRef {
        id: manifest.id,
        root: root.to_path_buf(),
        profile: root.join("web"),
        source_identity: manifest.source_identity,
        snapshot_identity: manifest.snapshot_identity,
        created_unix_ms: manifest.created_unix_ms,
    };
    verify_backup(shell_root, canonical_home, &backup).is_ok()
}

pub(super) fn backup_details(backup: &BackupRef) -> String {
    backup.root.display().to_string()
}

struct RecordLock {
    path: PathBuf,
    file: Option<fs::File>,
}

impl Drop for RecordLock {
    fn drop(&mut self) {
        self.file.take();
        if let Err(error) = fs::remove_file(&self.path) {
            eprintln!(
                "dsh-desktop: remove adoption append lock {}: {error}",
                self.path.display()
            );
        }
    }
}

fn acquire_record_lock(dir: &Path) -> Result<RecordLock, String> {
    fs::create_dir_all(dir).map_err(|error| {
        format!(
            "create adoption record directory {}: {error}",
            dir.display()
        )
    })?;
    let path = dir.join(RECORD_LOCK);
    let started = Instant::now();
    loop {
        match fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
        {
            Ok(mut file) => {
                writeln!(file, "{}", std::process::id())
                    .and_then(|()| file.sync_all())
                    .map_err(|error| {
                        format!(
                            "initialize adoption append lock {}: {error}",
                            path.display()
                        )
                    })?;
                return Ok(RecordLock {
                    path,
                    file: Some(file),
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let stale = fs::metadata(&path)
                    .and_then(|metadata| metadata.modified())
                    .and_then(|modified| modified.elapsed().map_err(std::io::Error::other))
                    .map(|age| age >= RECORD_LOCK_STALE)
                    .unwrap_or(false);
                if stale {
                    match fs::remove_file(&path) {
                        Ok(()) => continue,
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                        Err(_) => {}
                    }
                }
                if started.elapsed() >= RECORD_LOCK_WAIT {
                    return Err(format!(
                        "another Desktop process is updating adoption state for {}",
                        dir.display()
                    ));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => {
                return Err(format!(
                    "create adoption append lock {}: {error}",
                    path.display()
                ));
            }
        }
    }
}

fn append_record(
    shell_root: &Path,
    previous: Option<&AdoptionRecord>,
    record: &AdoptionRecord,
) -> Result<(), String> {
    validate_record(record, &record.dsh_home)?;
    let dir = shell_root
        .join(RECORDS_DIR)
        .join(home_key(&record.dsh_home));
    let _lock = acquire_record_lock(&dir)?;
    let latest = latest_record(shell_root, &record.dsh_home)?;
    match (previous, latest.as_ref()) {
        (None, None) => {}
        (Some(expected), Some(actual)) if expected.revision == actual.revision => {}
        _ => {
            return Err(format!(
                "adoption state changed concurrently for {}",
                record.dsh_home.display()
            ));
        }
    }
    let status = match record.status {
        AdoptionStatus::Adopting => "adopting",
        AdoptionStatus::Active => "active",
        AdoptionStatus::ConsentRequired => "consent-required",
        AdoptionStatus::RestorePending => "restore-pending",
        AdoptionStatus::Restored => "restored",
        AdoptionStatus::RestoreAbandoned => "restore-abandoned",
    };
    let nonce = FILE_NONCE.fetch_add(1, Ordering::Relaxed);
    let final_path = dir.join(format!(
        "{:020}-{status}-{}-{nonce}.json",
        record.revision,
        std::process::id()
    ));
    let temp = dir.join(format!(
        ".{:020}-{status}-{}-{nonce}.tmp",
        record.revision,
        std::process::id()
    ));
    let bytes = serde_json::to_vec_pretty(record)
        .map_err(|error| format!("serialize adoption record: {error}"))?;
    write_new_synced(&temp, &bytes)?;
    fs::rename(&temp, &final_path)
        .map_err(|error| format!("publish adoption record {}: {error}", final_path.display()))?;
    sync_directory(&dir)
}

fn validate_record(record: &AdoptionRecord, canonical_home: &Path) -> Result<(), String> {
    if record.schema != RECORD_SCHEMA
        || record.scope != CONSENT_SCOPE
        || record.dsh_home != canonical_home
        || record.revision == 0
    {
        return Err("adoption record does not match this DSH_HOME or schema".to_string());
    }
    if record.origin == AdoptionOrigin::ExistingHome && record.consented_unix_ms.is_none() {
        return Err("existing-home adoption lacks user consent timestamp".to_string());
    }
    if record.status == AdoptionStatus::RestorePending && record.restore_source_identity.is_none() {
        return Err("pending profile restore lacks its source identity".to_string());
    }
    Ok(())
}

fn is_meaningful_home_entry(name: &OsStr) -> bool {
    !matches!(
        name.to_str(),
        Some("logs" | ".DS_Store" | "Thumbs.db" | "desktop.ini")
    )
}

fn read_profile_plugins(profile: &Path) -> Result<Vec<String>, String> {
    let manifest = profile.join("package.json");
    if !manifest.is_file() {
        return Ok(Vec::new());
    }
    let text = fs::read_to_string(&manifest)
        .map_err(|error| format!("read web profile manifest {}: {error}", manifest.display()))?;
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|error| format!("parse web profile manifest {}: {error}", manifest.display()))?;
    let mut plugins = BTreeSet::new();
    if let Some(dependencies) = value
        .get("dependencies")
        .and_then(|value| value.as_object())
    {
        plugins.extend(dependencies.keys().cloned());
    }
    Ok(plugins.into_iter().collect())
}

fn count_agent_presets(root: &Path) -> Result<usize, String> {
    if !root.exists() {
        return Ok(0);
    }
    let entries = fs::read_dir(root)
        .map_err(|error| format!("read agent presets {}: {error}", root.display()))?;
    Ok(entries
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_dir())
        .count())
}

fn canonical_home(path: &Path) -> Result<PathBuf, String> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|current| current.join(path))
            .map_err(|error| format!("resolve relative DSH_HOME {}: {error}", path.display()))?
    };
    if absolute.exists() {
        return fs::canonicalize(&absolute)
            .map_err(|error| format!("resolve DSH_HOME {}: {error}", absolute.display()));
    }

    let mut ancestor = absolute.as_path();
    let mut suffix = Vec::new();
    while !ancestor.exists() {
        let name = ancestor
            .file_name()
            .ok_or_else(|| format!("DSH_HOME has no existing ancestor: {}", absolute.display()))?;
        suffix.push(name.to_os_string());
        ancestor = ancestor
            .parent()
            .ok_or_else(|| format!("DSH_HOME has no existing ancestor: {}", absolute.display()))?;
    }
    let mut resolved = fs::canonicalize(ancestor)
        .map_err(|error| format!("resolve DSH_HOME ancestor {}: {error}", ancestor.display()))?;
    for component in suffix.into_iter().rev() {
        resolved.push(component);
    }
    Ok(resolved)
}

fn backup_parent(shell_root: &Path, canonical_home: &Path) -> PathBuf {
    shell_root.join(BACKUPS_DIR).join(home_key(canonical_home))
}

fn home_key(path: &Path) -> String {
    let mut digest = Sha256::new();
    digest.update(b"dsh-desktop-adoption-home-v1\0");
    hash_os_str(&mut digest, path.as_os_str());
    format!("{:x}", digest.finalize())
}

fn bytes_fingerprint(bytes: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(b"dsh-desktop-profile-backup-manifest-v1\0");
    digest.update((bytes.len() as u64).to_le_bytes());
    digest.update(bytes);
    format!("{:x}", digest.finalize())
}

#[cfg(unix)]
fn hash_os_str(digest: &mut Sha256, value: &OsStr) {
    use std::os::unix::ffi::OsStrExt;
    digest.update((value.as_bytes().len() as u64).to_le_bytes());
    digest.update(value.as_bytes());
}

#[cfg(windows)]
fn hash_os_str(digest: &mut Sha256, value: &OsStr) {
    use std::os::windows::ffi::OsStrExt;
    let bytes = value
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    digest.update((bytes.len() as u64).to_le_bytes());
    digest.update(bytes);
}

fn validate_backup_id(id: &str) -> Result<(), String> {
    if !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit() || byte == b'-') {
        Ok(())
    } else {
        Err(format!("invalid profile backup id {id:?}"))
    }
}

fn unix_millis() -> Result<u64, String> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("read system time for profile adoption: {error}"))?
        .as_millis();
    u64::try_from(millis).map_err(|_| "system time does not fit u64 milliseconds".to_string())
}

fn write_synced(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut file =
        fs::File::create(path).map_err(|error| format!("create {}: {error}", path.display()))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("write {}: {error}", path.display()))
}

fn write_new_synced(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .map_err(|error| format!("create {}: {error}", path.display()))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("write {}: {error}", path.display()))
}

fn sync_tree(root: &Path) -> Result<(), String> {
    for entry in fs::read_dir(root).map_err(|error| format!("read {}: {error}", root.display()))? {
        let entry =
            entry.map_err(|error| format!("read entry under {}: {error}", root.display()))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|error| format!("read file type {}: {error}", path.display()))?;
        if file_type.is_dir() {
            sync_tree(&path)?;
        } else if file_type.is_file() {
            sync_backup_file(&path)?;
        }
    }
    sync_directory(root)
}

#[cfg(not(windows))]
fn sync_backup_file(path: &Path) -> Result<(), String> {
    fs::File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|error| format!("sync backup file {}: {error}", path.display()))
}

#[cfg(windows)]
fn sync_backup_file(path: &Path) -> Result<(), String> {
    match fs::OpenOptions::new().read(true).write(true).open(path) {
        Ok(file) => file
            .sync_all()
            .map_err(|error| format!("sync backup file {}: {error}", path.display())),
        Err(error)
            if error.kind() == std::io::ErrorKind::PermissionDenied
                && fs::metadata(path)
                    .map(|metadata| metadata.permissions().readonly())
                    .unwrap_or(false) =>
        {
            Ok(())
        }
        Err(error) => Err(format!(
            "open backup file {} for sync: {error}",
            path.display()
        )),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "dsh-desktop-profile-adoption-{}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let shell = root.join("shell");
        let home = root.join("home");
        fs::create_dir_all(&shell).unwrap();
        fs::create_dir_all(home.join("profiles/web/node_modules")).unwrap();
        fs::write(
            home.join("profiles/web/package.json"),
            "{\"dependencies\":{\"custom-plugin\":\"link:custom\"}}\n",
        )
        .unwrap();
        fs::write(home.join("profiles/web/pnpm-lock.yaml"), "lock\n").unwrap();
        fs::write(
            home.join("profiles/web/pnpm-workspace.yaml"),
            "packages:\n  - .\n",
        )
        .unwrap();
        fs::write(home.join("profiles/web/cordis.patch.yml"), "[]\n").unwrap();
        fs::write(
            home.join("profiles/web/node_modules/installed"),
            "ignored\n",
        )
        .unwrap();
        (shell, home)
    }

    #[test]
    fn inspection_ignores_platform_noise_in_an_otherwise_empty_home() {
        let root = std::env::temp_dir().join(format!(
            "dsh-desktop-profile-adoption-{}-noise",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let home = root.join("home");
        fs::create_dir_all(home.join("logs")).unwrap();
        fs::write(home.join(".DS_Store"), "noise").unwrap();
        fs::write(home.join("Thumbs.db"), "noise").unwrap();
        let summary = inspect_home(&home).unwrap();
        assert!(!summary.has_existing_data);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn inspection_counts_plugins_and_agent_presets() {
        let (shell, home) = scratch("inspect");
        fs::create_dir_all(home.join(".agent-presets/one")).unwrap();
        fs::create_dir_all(home.join(".agent-presets/two")).unwrap();
        let summary = inspect_home(&home).unwrap();
        assert!(summary.has_existing_data);
        assert!(summary.has_web_profile);
        assert_eq!(summary.plugins, ["custom-plugin"]);
        assert_eq!(summary.agent_preset_count, 2);
        fs::remove_dir_all(shell.parent().unwrap()).unwrap();
    }

    #[test]
    fn backup_is_complete_and_excludes_rebuildable_node_modules() {
        let (shell, home) = scratch("backup");
        let canonical = fs::canonicalize(&home).unwrap();
        let backup = create_backup(&shell, &canonical).unwrap();
        verify_backup(&shell, &canonical, &backup).unwrap();
        assert!(backup.profile.join("package.json").is_file());
        assert!(!backup.profile.join("node_modules").exists());
        assert!(current_profile_matches_backup(&canonical, &backup).unwrap());
        fs::write(home.join("profiles/web/package.json"), "{}\n").unwrap();
        assert!(!current_profile_matches_backup(&canonical, &backup).unwrap());
        fs::remove_dir_all(shell.parent().unwrap()).unwrap();
    }

    #[test]
    fn append_only_records_preserve_consent_and_transitions() {
        let (shell, home) = scratch("records");
        let canonical = fs::canonicalize(&home).unwrap();
        let backup = create_backup(&shell, &canonical).unwrap();
        let adopting = start_record(
            &shell,
            &canonical,
            AdoptionOrigin::ExistingHome,
            true,
            Some(backup.clone()),
        )
        .unwrap();
        let pending = begin_restore(&shell, &adopting, "current-profile".into()).unwrap();
        assert_eq!(
            pending.restore_source_identity.as_deref(),
            Some("current-profile")
        );
        let restored =
            transition(&shell, &pending, AdoptionStatus::Restored, Some(backup)).unwrap();
        assert_eq!(latest_record(&shell, &canonical).unwrap(), Some(restored));
        assert_eq!(
            fs::read_dir(shell.join(RECORDS_DIR).join(home_key(&canonical)))
                .unwrap()
                .count(),
            3
        );
        fs::remove_dir_all(shell.parent().unwrap()).unwrap();
    }

    #[test]
    fn invalid_and_duplicate_records_recover_through_fresh_consent() {
        let (shell, home) = scratch("record-recovery");
        let canonical = fs::canonicalize(&home).unwrap();
        let adopting =
            start_record(&shell, &canonical, AdoptionOrigin::FreshHome, false, None).unwrap();
        let dir = shell.join(RECORDS_DIR).join(home_key(&canonical));
        fs::write(dir.join("broken.json"), "not-json\n").unwrap();
        let recovered = latest_record(&shell, &canonical).unwrap().unwrap();
        assert_eq!(recovered.status, AdoptionStatus::ConsentRequired);
        assert_eq!(recovered.revision, adopting.revision);
        assert!(!dir.join("broken.json").exists());
        assert_eq!(latest_record(&shell, &canonical).unwrap(), Some(adopting));

        let original = fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .find(|entry| {
                entry.path().extension() == Some(OsStr::new("json"))
                    && entry.file_name() != "broken.json"
            })
            .unwrap()
            .path();
        fs::copy(&original, dir.join("duplicate.json")).unwrap();
        let recovered = latest_record(&shell, &canonical).unwrap().unwrap();
        assert_eq!(recovered.status, AdoptionStatus::ConsentRequired);
        assert!(recovered.backup.is_none());
        fs::remove_dir_all(shell.parent().unwrap()).unwrap();
    }

    #[test]
    fn append_lock_allows_only_one_concurrent_transition() {
        let (shell, home) = scratch("record-lock");
        let canonical = fs::canonicalize(&home).unwrap();
        let adopting =
            start_record(&shell, &canonical, AdoptionOrigin::FreshHome, false, None).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let handles = [AdoptionStatus::Active, AdoptionStatus::ConsentRequired]
            .into_iter()
            .map(|status| {
                let shell = shell.clone();
                let adopting = adopting.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    transition(&shell, &adopting, status, None)
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();
        let results = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(results.iter().filter(|result| result.is_err()).count(), 1);
        let record_count = fs::read_dir(shell.join(RECORDS_DIR).join(home_key(&canonical)))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension() == Some(OsStr::new("json")))
            .count();
        assert_eq!(record_count, 2);
        fs::remove_dir_all(shell.parent().unwrap()).unwrap();
    }

    #[test]
    fn restored_adoption_requires_fresh_consent_and_backup() {
        let (shell, home) = scratch("restart-after-restore");
        let canonical = fs::canonicalize(&home).unwrap();
        let first_backup = create_backup(&shell, &canonical).unwrap();
        let adopting = start_record(
            &shell,
            &canonical,
            AdoptionOrigin::ExistingHome,
            true,
            Some(first_backup.clone()),
        )
        .unwrap();
        assert!(restart_with_consent(&shell, &adopting, None).is_err());
        let restored = transition(
            &shell,
            &adopting,
            AdoptionStatus::Restored,
            Some(first_backup),
        )
        .unwrap();
        let second_backup = create_backup(&shell, &canonical).unwrap();
        let restarted =
            restart_with_consent(&shell, &restored, Some(second_backup.clone())).unwrap();
        assert_eq!(restarted.status, AdoptionStatus::Adopting);
        assert_eq!(restarted.origin, AdoptionOrigin::ExistingHome);
        assert!(restarted.consented_unix_ms.is_some());
        assert_eq!(restarted.backup, Some(second_backup));
        fs::remove_dir_all(shell.parent().unwrap()).unwrap();
    }

    #[test]
    fn backup_never_copies_sibling_home_data() {
        let (shell, home) = scratch("scope");
        fs::create_dir_all(home.join("sessions")).unwrap();
        fs::write(home.join("sessions/user.jsonl"), "private-session\n").unwrap();
        fs::create_dir_all(home.join(".agent-presets/private")).unwrap();
        fs::write(home.join(".agent-presets/private/cordis.yml"), "[]\n").unwrap();
        let canonical = fs::canonicalize(&home).unwrap();
        let backup = create_backup(&shell, &canonical).unwrap();
        assert!(!backup.root.join("sessions").exists());
        assert!(!backup.root.join(".agent-presets").exists());
        assert_eq!(
            fs::read_to_string(home.join("sessions/user.jsonl")).unwrap(),
            "private-session\n"
        );
        fs::remove_dir_all(shell.parent().unwrap()).unwrap();
    }

    #[test]
    fn tampered_backup_fails_verification() {
        let (shell, home) = scratch("tamper");
        let canonical = fs::canonicalize(&home).unwrap();
        let backup = create_backup(&shell, &canonical).unwrap();
        fs::write(backup.profile.join("package.json"), "{}\n").unwrap();
        assert!(verify_backup(&shell, &canonical, &backup)
            .unwrap_err()
            .contains("contents"));
        fs::remove_dir_all(shell.parent().unwrap()).unwrap();
    }
}
