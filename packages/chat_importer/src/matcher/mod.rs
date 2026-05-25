mod ios_sms;
mod ios_wc;
mod report;
mod utils;
mod win_qq_html;
mod win_qq_mht;

use base64::{engine::general_purpose::STANDARD, Engine};
use htmlescape::decode_html;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use lazy_static::lazy_static;
pub use log::{debug, error, info, warn};
use path_ext::PathExt;
use regex::{Captures, Regex};
use utils::{blob_dhash, hamming_distance};

use crate::store::{Attachments, ChatStore, MetadataMerger, Record, RecordType};
use assetpack_core::Hash32;
use ibackuptool2::Backup;
pub use report::{format_bytes, path_disk_bytes, ImportMetrics, ImportReport};

pub trait MsgMatcher {
    fn get_records(&self) -> Option<Vec<RecordType>>;
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

pub async fn exporter<P>(store: &mut ChatStore, export_type: ExportType<P>) -> Result<ImportReport>
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
    export_matcher(store, matcher.as_ref()).await
}

pub async fn export_ios_wechat_backup(
    store: &mut ChatStore,
    backup: Backup,
    names: Option<Vec<String>>,
) -> Result<ImportReport> {
    info!("Importing unlocked backup: {}", backup.path.display());
    let matcher = ios_wc::Matcher::from_backup(backup, names)?;
    export_matcher(store, matcher.as_ref()).await
}

pub async fn export_ios_sms_backup(
    store: &mut ChatStore,
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
    export_matcher(store, matcher.as_ref()).await
}

pub async fn export_matcher(
    store: &mut ChatStore,
    matcher: &dyn MsgMatcher,
) -> Result<ImportReport> {
    let records = matcher.get_records().context("Cannot transfrom records")?;
    let total = records.len();
    let blob_total = records
        .iter()
        .map(RecordType::attachment_count)
        .sum::<usize>();
    let merger = matcher.get_metadata_merger();
    let progress = PersistProgress::new(total as u64, blob_total as u64)?;
    let mut metrics = ImportMetrics {
        records_seen: total as u64,
        ..Default::default()
    };
    for (i, record) in records.into_iter().enumerate() {
        let display = record.display();
        let blob_progress = progress.blobs.clone();
        let outcome = store
            .insert_or_update_detailed(record, merger.as_deref(), || {
                blob_progress.inc(1);
            })
            .await
            .context(format!("Cannot insert records: {}", display))?;
        metrics.add_write_outcome(&outcome);
        progress.records.inc(1);
        progress.records.set_message(format!("{}/{}", i + 1, total));
    }
    progress.finish();
    Ok(ImportReport::imported(metrics))
}

struct PersistProgress {
    records: ProgressBar,
    blobs: ProgressBar,
    multi: MultiProgress,
}

impl PersistProgress {
    fn new(record_total: u64, blob_total: u64) -> Result<Self> {
        let multi = MultiProgress::new();
        let style = ProgressStyle::with_template(
            "{prefix:>7} [{bar:40.cyan/blue}] {pos}/{len} {percent:>3}% {elapsed_precise} {msg}",
        )?
        .progress_chars("=> ");
        let records = multi.add(ProgressBar::new(record_total));
        records.set_style(style.clone());
        records.set_prefix("chats");
        let blobs = multi.add(ProgressBar::new(blob_total));
        blobs.set_style(style);
        blobs.set_prefix("blobs");
        Ok(Self {
            records,
            blobs,
            multi,
        })
    }

    fn finish(self) {
        self.records.finish_and_clear();
        self.blobs.finish_and_clear();
        self.multi.clear().ok();
    }
}

impl Drop for PersistProgress {
    fn drop(&mut self) {
        self.records.finish_and_clear();
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
        fn get_records(&self) -> Option<Vec<RecordType>> {
            Some(self.records.clone())
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

        let report =
            export_ios_sms_backup(&mut store, &backup, "Owner".into(), None, "backup-1".into())
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
