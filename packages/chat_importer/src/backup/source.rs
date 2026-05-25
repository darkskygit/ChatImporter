use super::password::{PasswordInput, PasswordManager, PasswordResolution};
use anyhow::Context;
use ibackuptool2::{Backup, BackupError, BackupFileResolver, StandardBackupFileResolver};
use std::path::Path;
use std::sync::Arc;

#[derive(Debug)]
pub enum UnlockDecision {
    Unlocked(Box<Backup>),
    Skip,
}

#[derive(Debug)]
pub enum BackupOpenError {
    Open(anyhow::Error),
    MissingKeybag,
    InvalidPassword,
    UnlockFailed,
    ParseManifest(anyhow::Error),
    Prompt(anyhow::Error),
}

impl std::fmt::Display for BackupOpenError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Open(error) => write!(formatter, "failed to open backup: {:#}", error),
            Self::MissingKeybag => write!(formatter, "encrypted backup has no keybag"),
            Self::InvalidPassword => write!(formatter, "invalid backup password"),
            Self::UnlockFailed => write!(formatter, "failed to unlock encrypted backup manifest"),
            Self::ParseManifest(error) => {
                write!(formatter, "failed to parse backup manifest: {:#}", error)
            }
            Self::Prompt(error) => write!(formatter, "failed to read backup password: {:#}", error),
        }
    }
}

impl std::error::Error for BackupOpenError {}

#[cfg(test)]
pub fn open_backup(path: impl AsRef<Path>) -> anyhow::Result<Backup> {
    let mut password_manager = PasswordManager::new();
    match open_standard_backup_with_password_manager(path, &mut password_manager)? {
        UnlockDecision::Unlocked(backup) => Ok(*backup),
        UnlockDecision::Skip => Err(anyhow::anyhow!("backup skipped")),
    }
}

pub fn open_standard_backup_with_password_manager(
    path: impl AsRef<Path>,
    password_manager: &mut PasswordManager,
) -> std::result::Result<UnlockDecision, BackupOpenError> {
    open_backup_with_password_manager(path, Arc::new(StandardBackupFileResolver), password_manager)
}

pub fn open_backup_with_password_manager(
    path: impl AsRef<Path>,
    resolver: Arc<dyn BackupFileResolver>,
    password_manager: &mut PasswordManager,
) -> std::result::Result<UnlockDecision, BackupOpenError> {
    open_backup_with_password_manager_and_prompt(
        path,
        password_manager,
        || {
            rpassword::prompt_password("Backup Password (or 'skip'): ")
                .map_err(|e| BackupOpenError::Prompt(anyhow::anyhow!("{}", e)))
                .map(|password| PasswordManager::parse_input(&password))
        },
        resolver,
    )
}

pub(super) fn open_backup_with_password_manager_and_prompt<P>(
    path: impl AsRef<Path>,
    password_manager: &mut PasswordManager,
    prompt: P,
    resolver: Arc<dyn BackupFileResolver>,
) -> std::result::Result<UnlockDecision, BackupOpenError>
where
    P: FnMut() -> std::result::Result<PasswordInput, BackupOpenError>,
{
    let path = path.as_ref();
    let mut backup = Backup::new(path)
        .map_err(|e| anyhow::anyhow!("{}", e))
        .with_context(|| format!("failed to open backup root {}", path.display()))
        .map_err(BackupOpenError::Open)?
        .with_file_resolver(resolver.clone());

    if backup.manifest.is_encrypted {
        match password_manager.unlock_with(
            |password| {
                open_encrypted_backup(path, resolver.clone(), password).map_err(|error| {
                    log::warn!("failed to unlock encrypted backup: {}", error);
                    error
                })
            },
            prompt,
            is_retryable_password_error,
        )? {
            PasswordResolution::Unlocked(backup) => {
                return Ok(UnlockDecision::Unlocked(Box::new(backup)))
            }
            PasswordResolution::Skip => return Ok(UnlockDecision::Skip),
        }
    } else {
        backup
            .parse_manifest()
            .map_err(|e| anyhow::anyhow!("{}", e))
            .context("failed to parse backup manifest")
            .map_err(BackupOpenError::ParseManifest)?;
    }

    Ok(UnlockDecision::Unlocked(Box::new(backup)))
}

fn open_encrypted_backup(
    path: &Path,
    resolver: Arc<dyn BackupFileResolver>,
    password: &str,
) -> std::result::Result<Backup, BackupOpenError> {
    let mut backup = Backup::new(path)
        .map_err(|e| anyhow::anyhow!("{}", e))
        .with_context(|| format!("failed to open backup root {}", path.display()))
        .map_err(BackupOpenError::Open)?
        .with_file_resolver(resolver);

    backup
        .parse_keybag()
        .map_err(|e| anyhow::anyhow!("{}", e))
        .context("failed to parse encrypted backup keybag")
        .map_err(BackupOpenError::Open)?;
    if let Some(ref mut keybag) = backup.manifest.keybag.as_mut() {
        keybag
            .unlock_with_passcode(password)
            .map_err(map_backup_unlock_error)?;
    } else {
        return Err(BackupOpenError::MissingKeybag);
    }
    backup
        .manifest
        .unlock_manifest()
        .map_err(map_backup_unlock_error)?;
    backup
        .parse_manifest()
        .map_err(|e| anyhow::anyhow!("{}", e))
        .context("failed to parse encrypted backup manifest")
        .map_err(BackupOpenError::ParseManifest)?;

    Ok(backup)
}

fn map_backup_unlock_error(error: BackupError) -> BackupOpenError {
    match error {
        BackupError::InvalidPassword => BackupOpenError::InvalidPassword,
        _ => BackupOpenError::UnlockFailed,
    }
}

fn is_retryable_password_error(error: &BackupOpenError) -> bool {
    matches!(
        error,
        BackupOpenError::InvalidPassword
            | BackupOpenError::UnlockFailed
            | BackupOpenError::ParseManifest(_)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypted_manifest_parse_failure_is_retryable_as_password_candidate() {
        assert!(is_retryable_password_error(
            &BackupOpenError::ParseManifest(anyhow::anyhow!(
                "failed to parse encrypted backup manifest"
            ))
        ));
    }

    #[test]
    fn missing_keybag_and_open_errors_are_not_password_retries() {
        assert!(!is_retryable_password_error(
            &BackupOpenError::MissingKeybag
        ));
        assert!(!is_retryable_password_error(&BackupOpenError::Open(
            anyhow::anyhow!("not a backup")
        )));
    }
}
