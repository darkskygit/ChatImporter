mod args;
mod backup;
mod logger;
mod matcher;
mod store;

use anyhow::Result;
use args::{get_cmd, get_log_level, get_paths, SubCommand};
use backup::{
    open_backup_with_password_manager, open_standard_backup_with_password_manager,
    BackupCandidateKind, ImazingArchive, ImportSession, PasswordManager, UnlockDecision,
};
use logger::init_logger;
use matcher::{
    export_ios_sms_backup, export_ios_wechat_backup, exporter, format_bytes, info, path_disk_bytes,
    warn, ExportType, ImportMetrics, ImportReport,
};
use std::path::Path;
use store::ChatStore;

#[tokio::main]
async fn main() -> Result<()> {
    let progress = init_logger(get_log_level().to_level_filter())?;
    let mut store = ChatStore::open("record.db").await?;
    let paths = get_paths();
    match get_cmd() {
        SubCommand::QQ { owner, .. } => {
            let mut total_metrics = ImportMetrics::default();
            let mut imported = 0usize;
            let mut failed = 0usize;
            for path in paths {
                info!("Processing: {}", path.display());
                match exporter(
                    &mut store,
                    &progress,
                    ExportType::WindowsQQ(&path, owner.into()),
                )
                .await
                {
                    Ok(mut report) => {
                        report.metrics.source_path_disk_bytes = path_disk_bytes(&path);
                        log_import_report(&path, &report);
                        total_metrics.add(&report.metrics);
                        imported += 1;
                    }
                    Err(error) => {
                        warn!("failed to import {}: {}", path.display(), error);
                        failed += 1;
                    }
                }
            }
            info!(
                "Import summary: imported={}, skipped=0, failed={}, {}",
                imported,
                failed,
                metrics_summary(&total_metrics)
            );
        }
        SubCommand::WeChat { chat_names, .. } => {
            let names = chat_names.as_ref().map(|names| {
                if names.is_empty() {
                    Vec::new()
                } else {
                    names.split(',').map(|s| s.into()).collect()
                }
            });
            let mut session = ImportSession::discover(&paths)?;
            let mut password_manager = PasswordManager::new();
            let mut total_metrics = ImportMetrics::default();
            for candidate in session.candidates().to_vec() {
                info!("Processing: {}", candidate.display_path.display());
                match candidate.kind {
                    BackupCandidateKind::StandardRoot(path) => {
                        match open_standard_backup_with_password_manager(
                            &path,
                            &mut password_manager,
                        ) {
                            Ok(UnlockDecision::Unlocked(backup)) => match export_ios_wechat_backup(
                                &mut store,
                                &progress,
                                *backup,
                                names.clone(),
                            )
                            .await
                            {
                                Ok(report) => record_import_report(
                                    &mut session,
                                    &mut total_metrics,
                                    report,
                                    &candidate.display_path,
                                ),
                                Err(error) => {
                                    warn!(
                                        "failed to import backup candidate {} ({}): {}",
                                        candidate.source_id,
                                        candidate.display_path.display(),
                                        error
                                    );
                                    session.mark_failed();
                                }
                            },
                            Ok(UnlockDecision::Skip) => session.mark_skipped(),
                            Err(error) => {
                                warn!(
                                    "failed to open backup candidate {} ({}): {}",
                                    candidate.source_id,
                                    candidate.display_path.display(),
                                    error
                                );
                                session.mark_failed();
                            }
                        }
                    }
                    BackupCandidateKind::ImazingVersionsDb { versions_db } => {
                        match ImazingArchive::open(&versions_db)
                            .and_then(|archive| archive.version_candidates())
                        {
                            Ok(versions) if versions.is_empty() => session.mark_skipped(),
                            Ok(versions) => {
                                for version in versions {
                                    match open_backup_with_password_manager(
                                        &version.snapshot_dir,
                                        version.file_resolver(),
                                        &mut password_manager,
                                    ) {
                                        Ok(UnlockDecision::Unlocked(backup)) => {
                                            match export_ios_wechat_backup(
                                                &mut store,
                                                &progress,
                                                *backup,
                                                names.clone(),
                                            )
                                            .await
                                            {
                                                Ok(report) => record_import_report(
                                                    &mut session,
                                                    &mut total_metrics,
                                                    report,
                                                    &version.snapshot_dir,
                                                ),
                                                Err(error) => {
                                                    warn!(
                                                        "failed to import iMazing backup candidate {} ({}): {}",
                                                        version.source_id,
                                                        version.snapshot_dir.display(),
                                                        error
                                                    );
                                                    session.mark_failed();
                                                }
                                            }
                                        }
                                        Ok(UnlockDecision::Skip) => session.mark_skipped(),
                                        Err(error) => {
                                            warn!(
                                                "failed to open iMazing backup candidate {} ({}): {}",
                                                version.source_id,
                                                version.snapshot_dir.display(),
                                                error
                                            );
                                            session.mark_failed();
                                        }
                                    }
                                }
                            }
                            Err(error) => {
                                warn!(
                                    "failed to list iMazing version candidates from {}: {}",
                                    versions_db.display(),
                                    error
                                );
                                session.mark_failed();
                            }
                        }
                    }
                }
            }
            let summary = session.summary();
            info!(
                "Import summary: imported={}, skipped={}, failed={}, {}",
                summary.imported,
                summary.skipped,
                summary.failed,
                metrics_summary(&total_metrics)
            );
        }
        SubCommand::Sms {
            owner, owner_id, ..
        } => {
            let mut session = ImportSession::discover(&paths)?;
            let mut password_manager = PasswordManager::new();
            let mut total_metrics = ImportMetrics::default();
            for candidate in session.candidates().to_vec() {
                info!("Processing: {}", candidate.display_path.display());
                match candidate.kind {
                    BackupCandidateKind::StandardRoot(path) => {
                        match open_standard_backup_with_password_manager(
                            &path,
                            &mut password_manager,
                        ) {
                            Ok(UnlockDecision::Unlocked(backup)) => {
                                match export_ios_sms_backup(
                                    &mut store,
                                    &progress,
                                    backup.as_ref(),
                                    owner.into(),
                                    owner_id.clone(),
                                    candidate.source_id.clone(),
                                )
                                .await
                                {
                                    Ok(report) => record_import_report(
                                        &mut session,
                                        &mut total_metrics,
                                        report,
                                        &candidate.display_path,
                                    ),
                                    Err(error) => {
                                        warn!(
                                            "failed to import backup candidate {} ({}): {}",
                                            candidate.source_id,
                                            candidate.display_path.display(),
                                            error
                                        );
                                        session.mark_failed();
                                    }
                                }
                            }
                            Ok(UnlockDecision::Skip) => session.mark_skipped(),
                            Err(error) => {
                                warn!(
                                    "failed to open backup candidate {} ({}): {}",
                                    candidate.source_id,
                                    candidate.display_path.display(),
                                    error
                                );
                                session.mark_failed();
                            }
                        }
                    }
                    BackupCandidateKind::ImazingVersionsDb { versions_db } => {
                        match ImazingArchive::open(&versions_db)
                            .and_then(|archive| archive.version_candidates())
                        {
                            Ok(versions) if versions.is_empty() => session.mark_skipped(),
                            Ok(versions) => {
                                for version in versions {
                                    match open_backup_with_password_manager(
                                        &version.snapshot_dir,
                                        version.file_resolver(),
                                        &mut password_manager,
                                    ) {
                                        Ok(UnlockDecision::Unlocked(backup)) => {
                                            match export_ios_sms_backup(
                                                &mut store,
                                                &progress,
                                                backup.as_ref(),
                                                owner.clone(),
                                                owner_id.clone(),
                                                version.source_id.clone(),
                                            )
                                            .await
                                            {
                                                Ok(report) => record_import_report(
                                                    &mut session,
                                                    &mut total_metrics,
                                                    report,
                                                    &version.snapshot_dir,
                                                ),
                                                Err(error) => {
                                                    warn!(
                                                        "failed to import iMazing backup candidate {} ({}): {}",
                                                        version.source_id,
                                                        version.snapshot_dir.display(),
                                                        error
                                                    );
                                                    session.mark_failed();
                                                }
                                            }
                                        }
                                        Ok(UnlockDecision::Skip) => session.mark_skipped(),
                                        Err(error) => {
                                            warn!(
                                                "failed to open iMazing backup candidate {} ({}): {}",
                                                version.source_id,
                                                version.snapshot_dir.display(),
                                                error
                                            );
                                            session.mark_failed();
                                        }
                                    }
                                }
                            }
                            Err(error) => {
                                warn!(
                                    "failed to list iMazing version candidates from {}: {}",
                                    versions_db.display(),
                                    error
                                );
                                session.mark_failed();
                            }
                        }
                    }
                }
            }
            let summary = session.summary();
            info!(
                "Import summary: imported={}, skipped={}, failed={}, {}",
                summary.imported,
                summary.skipped,
                summary.failed,
                metrics_summary(&total_metrics)
            );
        }
    }
    Ok(())
}

fn record_import_report(
    session: &mut ImportSession,
    totals: &mut ImportMetrics,
    mut report: ImportReport,
    display_path: &Path,
) {
    report.metrics.source_path_disk_bytes = path_disk_bytes(display_path);
    log_import_report(display_path, &report);
    totals.add(&report.metrics);
    match report.disposition {
        crate::backup::CandidateDisposition::Imported => session.mark_imported(),
        crate::backup::CandidateDisposition::Skipped => session.mark_skipped(),
    }
}

fn log_import_report(path: &Path, report: &ImportReport) {
    info!(
        "Import report: path={}, disposition={:?}, {}",
        path.display(),
        report.disposition,
        metrics_summary(&report.metrics)
    );
}

fn metrics_summary(metrics: &ImportMetrics) -> String {
    format!(
        "path_size={}, chats={}/{}, records_seen={}, inserted={}, updated={}, blobs={}/{}, attachments_seen={}, blob_read={}, blob_original={}, exact_assets_new={}, assetpack_estimated={}, assetpack_new={}, assetpack_new_objects={}, canonical_assets_new={}, errors=parse:{}/blob:{}/write:{}",
        format_bytes(metrics.source_path_disk_bytes),
        metrics.chats_parsed,
        metrics.chats_planned,
        metrics.records_seen,
        metrics.records_inserted,
        metrics.records_updated,
        metrics.blobs_prepared,
        metrics.blobs_planned,
        metrics.attachments_seen,
        format_bytes(metrics.blob_read_bytes),
        format_bytes(metrics.attachment_original_bytes),
        metrics.exact_assets_new,
        format_bytes(metrics.assetpack_estimated_bytes),
        format_bytes(metrics.assetpack_new_stored_bytes),
        metrics.assetpack_new_objects,
        metrics.canonical_assets_new,
        metrics.parse_errors,
        metrics.blob_errors,
        metrics.write_errors,
    )
}
