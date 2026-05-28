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

use crate::store::{
    Attachment, Attachments, ChatStore, MetadataMergeContext, MetadataMerger, PreparedRecord,
    Record, RecordType,
};
use assetpack_core::Hash32;
use ibackuptool2::Backup;
pub use report::{format_bytes, path_disk_bytes, ImportMetrics, ImportReport};

#[derive(Clone, Debug, Default)]
pub struct ImportPlan {
    pub chats_total: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct RecordBatch {
    pub records: Vec<RecordType>,
}

pub trait RecordSink: Sync {
    fn push(&self, record: RecordType) -> Result<()>;
}

pub trait MsgMatcher {
    fn import_plan(&self) -> ImportPlan {
        ImportPlan::default()
    }

    fn get_record_batches(&self, progress: &PipelineProgress) -> Result<Vec<RecordBatch>>;

    fn stream_records(&self, progress: &PipelineProgress, sink: &dyn RecordSink) -> Result<()> {
        for batch in self.get_record_batches(progress)? {
            for record in batch.records {
                sink.push(record)?;
            }
        }
        Ok(())
    }

    #[cfg(test)]
    fn collect_records(&self) -> Result<Vec<RecordType>> {
        let records = std::sync::Mutex::new(Vec::new());
        self.stream_records(
            &PipelineProgress::hidden(),
            &CollectRecordSink { records: &records },
        )?;
        records
            .into_inner()
            .map_err(|_| anyhow::anyhow!("collect record sink poisoned"))
    }

    fn get_metadata_merger(&self) -> Option<Box<dyn MetadataMerger>> {
        None
    }
}

#[cfg(test)]
struct CollectRecordSink<'a> {
    records: &'a std::sync::Mutex<Vec<RecordType>>,
}

#[cfg(test)]
impl RecordSink for CollectRecordSink<'_> {
    fn push(&self, record: RecordType) -> Result<()> {
        self.records
            .lock()
            .map_err(|_| anyhow::anyhow!("collect record sink poisoned"))?
            .push(record);
        Ok(())
    }
}

use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::fs::read;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::TrySendError;
use std::time::Duration;

const WRITE_BATCH_SIZE: usize = 512;
const WRITE_BATCH_STAGED_BYTES: u64 = 64 * 1024 * 1024;
const RECORD_QUEUE_BOUND: usize = 1024;
const PREPARED_QUEUE_BOUND: usize = 128;
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
    let progress = std::sync::Arc::new(PipelineProgress::new(
        progress_target,
        matcher.import_plan(),
    )?);
    let merger = matcher.get_metadata_merger();
    let result = export_matcher_streaming(
        store,
        matcher,
        merger.as_deref(),
        std::sync::Arc::clone(&progress),
    )
    .await;
    let checkpoint_result = store.checkpoint_wal().await;
    progress.finish();
    match (result, checkpoint_result) {
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Ok(report), Ok(())) => Ok(report),
    }
}

async fn export_matcher_streaming(
    store: &mut ChatStore,
    matcher: &dyn MsgMatcher,
    merger: Option<&dyn MetadataMerger>,
    progress: std::sync::Arc<PipelineProgress>,
) -> Result<ImportReport> {
    let metrics = std::sync::Arc::new(std::sync::Mutex::new(ImportMetrics {
        chats_planned: progress.chats_length(),
        ..Default::default()
    }));
    let written = std::sync::Arc::new(AtomicU64::new(0));
    let handle = tokio::runtime::Handle::current();
    let pipeline_error = std::sync::Arc::new(std::sync::Mutex::new(None::<anyhow::Error>));
    let stop = std::sync::Arc::new(AtomicBool::new(false));
    let (record_tx, record_rx) =
        std::sync::mpsc::sync_channel::<SequencedRecord>(RECORD_QUEUE_BOUND);
    let (prepared_tx, prepared_rx) =
        std::sync::mpsc::sync_channel::<SequencedPreparedRecord>(PREPARED_QUEUE_BOUND);
    let record_rx = std::sync::Arc::new(std::sync::Mutex::new(record_rx));

    std::thread::scope(|scope| {
        let worker_count = std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(4);
        for _ in 0..worker_count {
            let record_rx = std::sync::Arc::clone(&record_rx);
            let prepared_tx = prepared_tx.clone();
            let worker_error = std::sync::Arc::clone(&pipeline_error);
            let worker_stop = std::sync::Arc::clone(&stop);
            let worker_progress = std::sync::Arc::clone(&progress);
            scope.spawn(move || loop {
                if worker_stop.load(Ordering::Relaxed) {
                    break;
                }
                let record = {
                    let receiver = match record_rx.lock() {
                        Ok(receiver) => receiver,
                        Err(_) => {
                            set_pipeline_error(
                                &worker_error,
                                anyhow::anyhow!("record queue poisoned"),
                            );
                            break;
                        }
                    };
                    receiver.recv()
                };
                let Ok(record) = record else {
                    break;
                };
                let attachments = record.record.attachment_count() as u64;
                worker_progress.blob_planned(attachments);
                match ChatStore::prepare_record(record.record) {
                    Ok(prepared) => {
                        worker_progress.blob_prepared(attachments);
                        if prepared_tx
                            .send(SequencedPreparedRecord {
                                sequence: record.sequence,
                                record: prepared,
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(error) => {
                        worker_stop.store(true, Ordering::Relaxed);
                        set_pipeline_error(&worker_error, error);
                        break;
                    }
                }
            });
        }
        drop(prepared_tx);

        let writer_error = std::sync::Arc::clone(&pipeline_error);
        let writer_stop = std::sync::Arc::clone(&stop);
        let writer_progress = std::sync::Arc::clone(&progress);
        let writer_metrics = std::sync::Arc::clone(&metrics);
        let writer_written = std::sync::Arc::clone(&written);
        let writer = scope.spawn(move || {
            PreparedRecordWriter {
                handle: &handle,
                store,
                merger,
                progress: &writer_progress,
                metrics: &writer_metrics,
                written: &writer_written,
                stop: &writer_stop,
            }
            .write(prepared_rx)
        });

        let sink = ChannelRecordSink {
            sender: record_tx,
            sequence: AtomicU64::new(0),
            stop: std::sync::Arc::clone(&stop),
        };
        if let Err(error) = matcher
            .stream_records(&progress, &sink)
            .context("Cannot transform records")
        {
            stop.store(true, Ordering::Relaxed);
            set_pipeline_error(&pipeline_error, error);
        }
        drop(sink);
        match writer.join() {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                stop.store(true, Ordering::Relaxed);
                set_pipeline_error(&writer_error, error);
            }
            Err(_) => {
                stop.store(true, Ordering::Relaxed);
                set_pipeline_error(&writer_error, anyhow::anyhow!("writer thread panicked"));
            }
        }
    });
    if let Some(error) = pipeline_error
        .lock()
        .map_err(|_| anyhow::anyhow!("pipeline error slot poisoned"))?
        .take()
    {
        return Err(error);
    }
    let mut metrics = std::sync::Arc::try_unwrap(metrics)
        .map_err(|_| anyhow::anyhow!("pipeline metrics still shared"))?
        .into_inner()
        .map_err(|_| anyhow::anyhow!("pipeline metrics poisoned"))?;
    metrics.chats_parsed = progress.chats_position();
    Ok(ImportReport::imported(metrics))
}

struct ChannelRecordSink {
    sender: std::sync::mpsc::SyncSender<SequencedRecord>,
    sequence: AtomicU64,
    stop: std::sync::Arc<AtomicBool>,
}

impl RecordSink for ChannelRecordSink {
    fn push(&self, record: RecordType) -> Result<()> {
        let record = SequencedRecord {
            sequence: self.sequence.fetch_add(1, Ordering::Relaxed),
            record,
        };
        let mut record = Some(record);
        loop {
            if self.stop.load(Ordering::Relaxed) {
                return Err(anyhow::anyhow!("record pipeline stopped"));
            }
            match self
                .sender
                .try_send(record.take().expect("sequenced record missing"))
            {
                Ok(()) => return Ok(()),
                Err(TrySendError::Full(value)) => {
                    record = Some(value);
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(TrySendError::Disconnected(_)) => {
                    return Err(anyhow::anyhow!("record pipeline closed"));
                }
            }
        }
    }
}

struct SequencedRecord {
    sequence: u64,
    record: RecordType,
}

struct SequencedPreparedRecord {
    sequence: u64,
    record: PreparedRecord,
}

fn set_pipeline_error(slot: &std::sync::Mutex<Option<anyhow::Error>>, error: anyhow::Error) {
    if let Ok(mut slot) = slot.lock() {
        if slot.is_none() {
            *slot = Some(error);
        }
    }
}

struct PreparedRecordWriter<'a> {
    handle: &'a tokio::runtime::Handle,
    store: &'a mut ChatStore,
    merger: Option<&'a dyn MetadataMerger>,
    progress: &'a PipelineProgress,
    metrics: &'a std::sync::Mutex<ImportMetrics>,
    written: &'a AtomicU64,
    stop: &'a AtomicBool,
}

impl PreparedRecordWriter<'_> {
    fn write(
        &mut self,
        prepared_rx: std::sync::mpsc::Receiver<SequencedPreparedRecord>,
    ) -> Result<()> {
        let mut batch = Vec::with_capacity(WRITE_BATCH_SIZE);
        let mut staged_bytes = 0_u64;
        let mut pending = BTreeMap::<u64, PreparedRecord>::new();
        let mut next_sequence = 0_u64;
        for prepared in prepared_rx {
            pending.insert(prepared.sequence, prepared.record);
            self.drain_ordered(
                &mut pending,
                &mut next_sequence,
                &mut batch,
                &mut staged_bytes,
            )?;
            if self.stop.load(Ordering::Relaxed) {
                return Ok(());
            }
        }
        while pending.contains_key(&next_sequence) {
            self.drain_ordered(
                &mut pending,
                &mut next_sequence,
                &mut batch,
                &mut staged_bytes,
            )?;
            if self.stop.load(Ordering::Relaxed) {
                return Ok(());
            }
        }
        self.write_batch(&mut batch)
    }

    fn drain_ordered(
        &mut self,
        pending: &mut BTreeMap<u64, PreparedRecord>,
        next_sequence: &mut u64,
        batch: &mut Vec<PreparedRecord>,
        staged_bytes: &mut u64,
    ) -> Result<()> {
        while let Some(prepared) = pending.remove(next_sequence) {
            {
                let mut metrics = self
                    .metrics
                    .lock()
                    .map_err(|_| anyhow::anyhow!("pipeline metrics poisoned"))?;
                metrics.records_seen += 1;
                metrics.blobs_planned += prepared.attachments_seen() as u64;
                metrics.blobs_prepared += prepared.attachments_seen() as u64;
            }
            self.progress.record_planned(1);
            *staged_bytes += prepared.staged_bytes();
            batch.push(prepared);
            *next_sequence += 1;
            if batch.len() >= WRITE_BATCH_SIZE || *staged_bytes >= WRITE_BATCH_STAGED_BYTES {
                self.write_batch(batch)?;
                *staged_bytes = 0;
            }
        }
        Ok(())
    }

    fn write_batch(&mut self, batch: &mut Vec<PreparedRecord>) -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }
        let prepared = std::mem::take(batch);
        let outcomes =
            match self
                .handle
                .block_on(self.store.insert_or_update_prepared_batch_detailed(
                    prepared,
                    self.merger,
                    || {},
                )) {
                Ok(outcomes) => outcomes,
                Err(error) => {
                    self.stop.store(true, Ordering::Relaxed);
                    return Err(error).context("Cannot insert record batch");
                }
            };
        for outcome in outcomes {
            let records_seen = {
                let mut metrics = self
                    .metrics
                    .lock()
                    .map_err(|_| anyhow::anyhow!("pipeline metrics poisoned"))?;
                metrics.add_write_outcome(&outcome);
                metrics.records_seen
            };
            let current = self.written.fetch_add(1, Ordering::Relaxed) + 1;
            self.progress.record_written(current, records_seen);
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) fn test_progress() -> MultiProgress {
    let progress = MultiProgress::new();
    progress.set_draw_target(indicatif::ProgressDrawTarget::hidden());
    progress
}

pub struct PipelineProgress {
    parse: ProgressBar,
    chunks: ProgressBar,
    write: ProgressBar,
    blobs: ProgressBar,
    multi: MultiProgress,
    chunks_planned: AtomicU64,
    chunks_done: AtomicU64,
    blobs_planned: AtomicU64,
    records_planned: AtomicU64,
    hidden: bool,
}

impl PipelineProgress {
    #[cfg(test)]
    fn hidden() -> Self {
        Self {
            parse: ProgressBar::hidden(),
            chunks: ProgressBar::hidden(),
            write: ProgressBar::hidden(),
            blobs: ProgressBar::hidden(),
            multi: MultiProgress::new(),
            chunks_planned: AtomicU64::new(0),
            chunks_done: AtomicU64::new(0),
            blobs_planned: AtomicU64::new(0),
            records_planned: AtomicU64::new(0),
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
        let chunks = multi.add(ProgressBar::new(0));
        chunks.set_style(style.clone());
        chunks.set_prefix("chunks");
        let blobs = multi.add(ProgressBar::new(0));
        blobs.set_style(style.clone());
        blobs.set_prefix("prepare");
        let write = multi.add(ProgressBar::new(0));
        write.set_style(style);
        write.set_prefix("write");
        let progress = Self {
            parse,
            chunks,
            write,
            blobs,
            multi,
            chunks_planned: AtomicU64::new(0),
            chunks_done: AtomicU64::new(0),
            blobs_planned: AtomicU64::new(0),
            records_planned: AtomicU64::new(0),
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

    pub fn chat_chunk_parsed(&self, records: u64, blobs: u64) {
        let done = self.chunks_done.fetch_add(1, Ordering::Relaxed) + 1;
        self.chunks.set_position(done);
        self.chunks
            .set_message(format!("records={records} blobs={blobs}"));
    }

    pub fn chat_chunk_planned(&self) {
        let planned = self.chunks_planned.fetch_add(1, Ordering::Relaxed) + 1;
        self.chunks.set_length(planned);
    }

    pub fn blob_planned(&self, n: u64) {
        let planned = self.blobs_planned.fetch_add(n, Ordering::Relaxed) + n;
        self.blobs.set_length(planned);
        self.blobs.set_message("preparing");
    }

    pub fn blob_prepared(&self, n: u64) {
        self.blobs.inc(n);
    }

    pub fn record_planned(&self, n: u64) {
        let planned = self.records_planned.fetch_add(n, Ordering::Relaxed) + n;
        self.write.set_length(planned);
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

    fn finish(&self) {
        if self.hidden {
            return;
        }
        self.parse.finish_and_clear();
        self.chunks.finish_and_clear();
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
        self.chunks.finish_and_clear();
        self.write.finish_and_clear();
        self.blobs.finish_and_clear();
        self.multi.clear().ok();
    }
}

fn gen_md5<S: ToString>(user_name: S) -> String {
    use md5::{Digest, Md5};
    hex::encode(Md5::digest(user_name.to_string().as_bytes()))
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
        .map(|attachment| Hash32::sha3_256(attachment.bytes()).to_hex())
        .collect::<Vec<_>>();
    hashes.sort();
    Hash32::sha3_256(hashes.join("\n").as_bytes()).to_hex()
}

fn record_blob_count(records: &[RecordType]) -> usize {
    records.iter().map(RecordType::attachment_count).sum()
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
    use crate::store::Query;
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
                records: self.records.clone(),
            }])
        }
    }

    struct FailingStreamMatcher {
        record: RecordType,
    }

    impl MsgMatcher for FailingStreamMatcher {
        fn get_record_batches(&self, _progress: &PipelineProgress) -> Result<Vec<RecordBatch>> {
            unreachable!("failing stream matcher uses stream_records")
        }

        fn stream_records(
            &self,
            _progress: &PipelineProgress,
            sink: &dyn RecordSink,
        ) -> Result<()> {
            sink.push(self.record.clone())?;
            Err(anyhow::anyhow!("stream failed"))
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
                    vec![(
                        "asset.bin".into(),
                        Attachment::from_bytes(attachment.clone()),
                    )]
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

    #[tokio::test]
    async fn export_matcher_preserves_parse_order_after_parallel_prepare() {
        let dir = tempdir().unwrap();
        let mut store = ChatStore::open(dir.path().join("record.db")).await.unwrap();
        let record = |content: &str| Record {
            chat_type: "test".into(),
            owner_id: "owner".into(),
            group_id: "group".into(),
            sender_id: "sender".into(),
            sender_name: "Sender".into(),
            content: content.into(),
            timestamp: 1,
            ..Default::default()
        };
        let slow_first = RecordType::from((
            record("first"),
            vec![(
                "asset.bin".into(),
                Attachment::from_bytes(vec![7; 2 * 1024 * 1024]),
            )]
            .into_iter()
            .collect(),
        ));
        let fast_second = RecordType::from(record("second"));

        export_matcher(
            &mut store,
            &test_progress(),
            &TestMatcher {
                records: vec![slow_first, fast_second],
            },
        )
        .await
        .unwrap();

        let records = store.query(Query::default()).await.unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].content, "second");
    }

    #[tokio::test]
    async fn export_matcher_discards_pending_batch_after_stream_error() {
        let dir = tempdir().unwrap();
        let mut store = ChatStore::open(dir.path().join("record.db")).await.unwrap();
        let record = Record {
            chat_type: "test".into(),
            owner_id: "owner".into(),
            group_id: "group".into(),
            sender_id: "sender".into(),
            sender_name: "Sender".into(),
            content: "pending".into(),
            timestamp: 1,
            ..Default::default()
        };

        let error = export_matcher(
            &mut store,
            &test_progress(),
            &FailingStreamMatcher {
                record: RecordType::from(record),
            },
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("Cannot transform records"));
        assert!(store.query(Query::default()).await.unwrap().is_empty());
    }
}
