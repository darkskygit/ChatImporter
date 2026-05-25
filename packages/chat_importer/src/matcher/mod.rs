mod ios_sms;
mod ios_wc;
mod report;
mod win_qq_html;
mod win_qq_mht;

use base64::{engine::general_purpose::STANDARD, Engine};
use htmlescape::decode_html;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use lazy_static::lazy_static;
pub use log::{debug, error, info, warn};
use path_ext::PathExt;
use regex::{Captures, Regex};

use crate::store::{Attachments, ChatStore, MetadataMerger, PreparedRecord, Record, RecordType};
use assetpack_core::Hash32;
use ibackuptool2::Backup;
pub use report::{format_bytes, path_disk_bytes, ImportMetrics, ImportReport};

#[derive(Clone, Debug, Default)]
pub struct ImportPlan {
    pub chats_total: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct RecordBatch {
    pub label: String,
    pub records: Vec<RecordType>,
}

pub trait MsgMatcher {
    fn import_plan(&self) -> ImportPlan {
        ImportPlan::default()
    }

    fn get_record_batches(&self, progress: &PipelineProgress) -> Result<Vec<RecordBatch>>;

    #[cfg(test)]
    fn collect_records(&self) -> Result<Vec<RecordType>> {
        Ok(self
            .get_record_batches(&PipelineProgress::hidden())?
            .into_iter()
            .flat_map(|batch| batch.records)
            .collect())
    }

    fn get_metadata_merger(&self) -> Option<Box<dyn MetadataMerger>> {
        None
    }
}

use anyhow::{Context, Result};
use std::fs::read;
use std::path::Path;
#[allow(non_camel_case_types)]
pub enum ExportType<P: AsRef<Path>> {
    WindowsQQ(P, String),
}

pub async fn exporter<P>(
    store: &mut ChatStore,
    progress: &MultiProgress,
    export_type: ExportType<P>,
) -> Result<ImportReport>
where
    P: AsRef<Path>,
{
    let matcher = match export_type {
        ExportType::WindowsQQ(path, owner) => win_qq_mht::Matcher::from_mht(
            &read(&path)?,
            owner,
            path.as_ref()
                .file_stem()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .into(),
        )?,
    };
    export_matcher(store, progress, matcher.as_ref()).await
}

pub async fn export_ios_wechat_backup(
    store: &mut ChatStore,
    progress: &MultiProgress,
    backup: Backup,
    names: Option<Vec<String>>,
) -> Result<ImportReport> {
    info!("Importing unlocked backup: {}", backup.path.display());
    let matcher = ios_wc::Matcher::from_backup(backup, names)?;
    export_matcher(store, progress, matcher.as_ref()).await
}

pub async fn export_ios_sms_backup(
    store: &mut ChatStore,
    progress: &MultiProgress,
    backup: &Backup,
    owner_name: String,
    owner_id: Option<String>,
    source_backup_id: String,
) -> Result<ImportReport> {
    info!("Importing unlocked backup: {}", backup.path.display());
    let Some(matcher) =
        ios_sms::Matcher::from_backup(backup, owner_name, owner_id, source_backup_id)?
    else {
        return Ok(ImportReport::skipped());
    };
    export_matcher(store, progress, matcher.as_ref()).await
}

pub async fn export_matcher(
    store: &mut ChatStore,
    progress_target: &MultiProgress,
    matcher: &dyn MsgMatcher,
) -> Result<ImportReport> {
    let progress = PipelineProgress::new(progress_target, matcher.import_plan())?;
    let records = matcher
        .get_record_batches(&progress)
        .context("Cannot transform records")?
        .into_iter()
        .flat_map(|batch| batch.records)
        .collect::<Vec<_>>();
    let blob_total = records
        .iter()
        .map(RecordType::attachment_count)
        .sum::<usize>();
    progress.set_prepare_total(blob_total as u64);
    let prepared_records = prepare_records_parallel(records, &progress)?;
    let merger = matcher.get_metadata_merger();
    let mut metrics = ImportMetrics {
        chats_planned: progress.chats_length(),
        chats_parsed: progress.chats_position(),
        records_seen: prepared_records.len() as u64,
        blobs_planned: blob_total as u64,
        blobs_prepared: blob_total as u64,
        ..Default::default()
    };
    progress.set_write_totals(prepared_records.len() as u64);
    for (i, record) in prepared_records.into_iter().enumerate() {
        let display = record.display();
        let outcome = store
            .insert_or_update_prepared_detailed(record, merger.as_deref(), || {})
            .await
            .context(format!("Cannot insert records: {}", display))?;
        metrics.add_write_outcome(&outcome);
        progress.record_written(i as u64 + 1, metrics.records_seen);
    }
    progress.finish();
    Ok(ImportReport::imported(metrics))
}

#[cfg(test)]
pub(crate) fn test_progress() -> MultiProgress {
    let progress = MultiProgress::new();
    progress.set_draw_target(indicatif::ProgressDrawTarget::hidden());
    progress
}

pub struct PipelineProgress {
    parse: ProgressBar,
    write: ProgressBar,
    blobs: ProgressBar,
    multi: MultiProgress,
    hidden: bool,
}

impl PipelineProgress {
    #[cfg(test)]
    fn hidden() -> Self {
        Self {
            parse: ProgressBar::hidden(),
            write: ProgressBar::hidden(),
            blobs: ProgressBar::hidden(),
            multi: MultiProgress::new(),
            hidden: true,
        }
    }

    fn new(progress: &MultiProgress, plan: ImportPlan) -> Result<Self> {
        let multi = progress.clone();
        let style = ProgressStyle::with_template(
            "{prefix:>7} [{bar:40.cyan/blue}] {pos}/{len} {percent:>3}% {elapsed_precise} {msg}",
        )?
        .progress_chars("=> ");
        let parse = multi.add(ProgressBar::new(plan.chats_total.unwrap_or(0)));
        parse.set_style(style.clone());
        parse.set_prefix("parse");
        let blobs = multi.add(ProgressBar::new(0));
        blobs.set_style(style.clone());
        blobs.set_prefix("prepare");
        let write = multi.add(ProgressBar::new(0));
        write.set_style(style);
        write.set_prefix("write");
        let progress = Self {
            parse,
            write,
            blobs,
            multi,
            hidden: false,
        };
        Ok(progress)
    }

    pub fn chat_planned(&self, n: u64) {
        self.parse.set_length(n);
    }

    pub fn chat_parsed(&self, records: u64, blobs: u64) {
        self.parse.inc(1);
        self.parse
            .set_message(format!("records={records} blobs={blobs}"));
    }

    pub fn set_prepare_total(&self, blobs: u64) {
        self.blobs.set_length(blobs);
        self.blobs.set_position(0);
        self.blobs.set_message("preparing");
    }

    pub fn blob_prepared(&self, n: u64) {
        self.blobs.inc(n);
    }

    pub fn set_write_totals(&self, records: u64) {
        self.write.set_length(records);
    }

    pub fn record_written(&self, current: u64, total: u64) {
        self.write.inc(1);
        self.write.set_message(format!("{current}/{total}"));
    }

    pub fn chats_length(&self) -> u64 {
        self.parse.length().unwrap_or(0)
    }

    pub fn chats_position(&self) -> u64 {
        self.parse.position()
    }

    fn finish(self) {
        if self.hidden {
            return;
        }
        self.parse.finish_and_clear();
        self.write.finish_and_clear();
        self.blobs.finish_and_clear();
        self.multi.clear().ok();
    }
}

impl Drop for PipelineProgress {
    fn drop(&mut self) {
        if self.hidden {
            return;
        }
        self.parse.finish_and_clear();
        self.write.finish_and_clear();
        self.blobs.finish_and_clear();
        self.multi.clear().ok();
    }
}

fn gen_md5<S: ToString>(user_name: S) -> String {
    use md5::{Digest, Md5};
    format!("{:x}", Md5::digest(user_name.to_string().as_bytes()))
}

fn hex2b64(hex: &str) -> String {
    hex::decode(hex)
        .map(|h| STANDARD.encode(&h))
        .unwrap_or_else(|_| hex.into())
}

fn attachment_fingerprint(attachments: &Attachments) -> String {
    // Sort per-attachment hashes so the record identity stays stable if a parser yields the same
    // attachments in a different map order.
    let mut hashes = attachments
        .values()
        .map(|bytes| Hash32::sha3_256(bytes).to_hex())
        .collect::<Vec<_>>();
    hashes.sort();
    Hash32::sha3_256(hashes.join("\n").as_bytes()).to_hex()
}

fn record_blob_count(records: &[RecordType]) -> usize {
    records.iter().map(RecordType::attachment_count).sum()
}

fn prepare_records_parallel(
    records: Vec<RecordType>,
    progress: &PipelineProgress,
) -> Result<Vec<PreparedRecord>> {
    if records.len() <= 1 {
        return records
            .into_iter()
            .map(|record| {
                let attachments = record.attachment_count() as u64;
                let prepared = ChatStore::prepare_record(record)?;
                progress.blob_prepared(attachments);
                Ok(prepared)
            })
            .collect();
    }

    let worker_count = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(4)
        .min(records.len());
    let jobs = std::sync::Arc::new(std::sync::Mutex::new(
        records
            .into_iter()
            .enumerate()
            .collect::<std::collections::VecDeque<_>>(),
    ));
    let prepared =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::<(usize, PreparedRecord)>::new()));
    let errors = std::sync::Arc::new(std::sync::Mutex::new(Vec::<anyhow::Error>::new()));

    std::thread::scope(|scope| {
        for _ in 0..worker_count {
            let jobs = std::sync::Arc::clone(&jobs);
            let prepared = std::sync::Arc::clone(&prepared);
            let errors = std::sync::Arc::clone(&errors);
            scope.spawn(move || loop {
                let Some((index, record)) = jobs
                    .lock()
                    .expect("record prepare job queue poisoned")
                    .pop_front()
                else {
                    break;
                };
                let attachments = record.attachment_count() as u64;
                match ChatStore::prepare_record(record) {
                    Ok(record) => {
                        progress.blob_prepared(attachments);
                        prepared
                            .lock()
                            .expect("record prepare result queue poisoned")
                            .push((index, record));
                    }
                    Err(error) => errors
                        .lock()
                        .expect("record prepare error queue poisoned")
                        .push(error),
                }
            });
        }
    });

    let errors = std::sync::Arc::try_unwrap(errors)
        .map_err(|_| anyhow::anyhow!("record prepare error queue still shared"))?
        .into_inner()
        .map_err(|_| anyhow::anyhow!("record prepare error queue poisoned"))?;
    if let Some(error) = errors.into_iter().next() {
        return Err(error);
    }

    let mut prepared = std::sync::Arc::try_unwrap(prepared)
        .map_err(|_| anyhow::anyhow!("record prepare result queue still shared"))?
        .into_inner()
        .map_err(|_| anyhow::anyhow!("record prepare result queue poisoned"))?;
    prepared.sort_by_key(|(index, _)| *index);
    Ok(prepared.into_iter().map(|(_, record)| record).collect())
}

fn modify_timestamp(record_type: RecordType, near_sec: Option<i64>) -> Option<RecordType> {
    use std::cmp::max;
    if let Some(near_sec) = near_sec {
        match record_type {
            RecordType::Record(record) => Some(RecordType::from(Record {
                timestamp: max(near_sec, record.timestamp) + 1,
                ..record
            })),
            RecordType::RecordWithAttachments {
                record,
                attachments,
            } => Some(RecordType::from((
                Record {
                    timestamp: max(near_sec, record.timestamp) + 1,
                    ..record
                },
                attachments,
            ))),
        }
    } else {
        Some(record_type)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backup::CandidateDisposition;
    use plist::{Dictionary, Value};
    use tempfile::tempdir;

    struct TestMatcher {
        records: Vec<RecordType>,
    }

    impl MsgMatcher for TestMatcher {
        fn import_plan(&self) -> ImportPlan {
            ImportPlan {
                chats_total: Some(1),
            }
        }

        fn get_record_batches(&self, progress: &PipelineProgress) -> Result<Vec<RecordBatch>> {
            progress.chat_parsed(
                self.records.len() as u64,
                record_blob_count(&self.records) as u64,
            );
            Ok(vec![RecordBatch {
                label: "test".into(),
                records: self.records.clone(),
            }])
        }
    }

    fn write_plist(path: impl AsRef<std::path::Path>, dict: Dictionary) {
        plist::to_file_xml(path, &Value::Dictionary(dict)).unwrap();
    }

    fn backup_without_sms_owner_identity() -> tempfile::TempDir {
        let dir = tempdir().unwrap();

        let mut status = Dictionary::new();
        status.insert("BackupState".into(), "new".into());
        status.insert("Date".into(), "2026-05-25".into());
        status.insert("IsFullBackup".into(), true.into());
        status.insert("SnapshotState".into(), "finished".into());
        status.insert("UUID".into(), "backup-uuid".into());
        status.insert("Version".into(), "2.4".into());
        write_plist(dir.path().join("Status.plist"), status);

        let mut info = Dictionary::new();
        info.insert("Product Type".into(), "iPhone".into());
        info.insert("Product Version".into(), "18.0".into());
        info.insert("Target Identifier".into(), "".into());
        info.insert("Target Type".into(), "Device".into());
        write_plist(dir.path().join("Info.plist"), info);

        let mut lockdown = Dictionary::new();
        lockdown.insert("ProductVersion".into(), "18.0".into());
        lockdown.insert("ProductType".into(), "iPhone".into());
        lockdown.insert("UniqueDeviceID".into(), "".into());
        lockdown.insert("SerialNumber".into(), "serial".into());
        lockdown.insert("DeviceName".into(), "Test Phone".into());
        let mut manifest = Dictionary::new();
        manifest.insert("IsEncrypted".into(), false.into());
        manifest.insert("Version".into(), "9.1".into());
        manifest.insert("Date".into(), "2026-05-25".into());
        manifest.insert("SystemDomainsVersion".into(), "20".into());
        manifest.insert("WasPasscodeSet".into(), false.into());
        manifest.insert("Lockdown".into(), Value::Dictionary(lockdown));
        write_plist(dir.path().join("Manifest.plist"), manifest);

        dir
    }

    #[tokio::test]
    async fn ios_sms_export_returns_skipped_without_owner_or_device_id() {
        let backup_dir = backup_without_sms_owner_identity();
        let backup = Backup::new(backup_dir.path()).unwrap();
        let store_dir = tempdir().unwrap();
        let mut store = ChatStore::open(store_dir.path().join("record.db"))
            .await
            .unwrap();

        let report = export_ios_sms_backup(
            &mut store,
            &test_progress(),
            &backup,
            "Owner".into(),
            None,
            "backup-1".into(),
        )
        .await
        .unwrap();

        assert_eq!(report.disposition, CandidateDisposition::Skipped);
    }

    #[tokio::test]
    async fn export_matcher_reports_record_and_asset_metrics() {
        let dir = tempdir().unwrap();
        let mut store = ChatStore::open(dir.path().join("record.db")).await.unwrap();
        let record = Record {
            chat_type: "test".into(),
            owner_id: "owner".into(),
            group_id: "group".into(),
            sender_id: "sender".into(),
            sender_name: "Sender".into(),
            content: "with asset".into(),
            timestamp: 1,
            ..Default::default()
        };
        let attachment = b"asset bytes".to_vec();
        let report = export_matcher(
            &mut store,
            &test_progress(),
            &TestMatcher {
                records: vec![RecordType::from((
                    record,
                    vec![("asset.bin".into(), attachment.clone())]
                        .into_iter()
                        .collect(),
                ))],
            },
        )
        .await
        .unwrap();

        assert_eq!(report.disposition, CandidateDisposition::Imported);
        assert_eq!(report.metrics.records_seen, 1);
        assert_eq!(report.metrics.records_inserted, 1);
        assert_eq!(report.metrics.attachments_seen, 1);
        assert_eq!(
            report.metrics.attachment_original_bytes,
            attachment.len() as u64
        );
        assert_eq!(report.metrics.exact_assets_seen, 1);
        assert_eq!(report.metrics.exact_assets_new, 1);
        assert!(report.metrics.assetpack_new_stored_bytes > 0);
    }
}
