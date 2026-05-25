use std::fs::metadata;
use std::path::Path;

use walkdir::WalkDir;

use crate::backup::CandidateDisposition;
use crate::store::WriteOutcome;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ImportMetrics {
    pub source_path_disk_bytes: u64,
    pub chats_planned: u64,
    pub chats_parsed: u64,
    pub records_seen: u64,
    pub records_inserted: u64,
    pub records_updated: u64,
    pub blobs_planned: u64,
    pub blobs_prepared: u64,
    pub blob_read_bytes: u64,
    pub attachments_seen: u64,
    pub attachment_original_bytes: u64,
    pub exact_assets_seen: u64,
    pub exact_asset_original_bytes: u64,
    pub exact_assets_new: u64,
    pub assetpack_estimated_bytes: u64,
    pub assetpack_new_stored_bytes: u64,
    pub assetpack_new_objects: u64,
    pub canonical_assets_seen: u64,
    pub canonical_assets_new: u64,
    pub parse_errors: u64,
    pub blob_errors: u64,
    pub write_errors: u64,
}

impl ImportMetrics {
    pub fn add(&mut self, other: &Self) {
        self.source_path_disk_bytes += other.source_path_disk_bytes;
        self.chats_planned += other.chats_planned;
        self.chats_parsed += other.chats_parsed;
        self.records_seen += other.records_seen;
        self.records_inserted += other.records_inserted;
        self.records_updated += other.records_updated;
        self.blobs_planned += other.blobs_planned;
        self.blobs_prepared += other.blobs_prepared;
        self.blob_read_bytes += other.blob_read_bytes;
        self.attachments_seen += other.attachments_seen;
        self.attachment_original_bytes += other.attachment_original_bytes;
        self.exact_assets_seen += other.exact_assets_seen;
        self.exact_asset_original_bytes += other.exact_asset_original_bytes;
        self.exact_assets_new += other.exact_assets_new;
        self.assetpack_estimated_bytes += other.assetpack_estimated_bytes;
        self.assetpack_new_stored_bytes += other.assetpack_new_stored_bytes;
        self.assetpack_new_objects += other.assetpack_new_objects;
        self.canonical_assets_seen += other.canonical_assets_seen;
        self.canonical_assets_new += other.canonical_assets_new;
        self.parse_errors += other.parse_errors;
        self.blob_errors += other.blob_errors;
        self.write_errors += other.write_errors;
    }

    pub(super) fn add_write_outcome(&mut self, outcome: &WriteOutcome) {
        self.records_inserted += u64::from(outcome.record_inserted);
        self.records_updated += u64::from(outcome.record_updated);
        self.attachments_seen += outcome.attachments_seen as u64;
        self.attachment_original_bytes += outcome.attachment_original_bytes;
        self.blob_read_bytes += outcome.attachment_original_bytes;
        for asset in &outcome.assets {
            self.exact_assets_seen += 1;
            self.exact_asset_original_bytes += asset.original_bytes;
            self.exact_assets_new += u64::from(asset.exact_asset_new);
            self.assetpack_estimated_bytes += asset.estimated_stored_bytes;
            self.assetpack_new_stored_bytes += asset.new_stored_bytes;
            self.assetpack_new_objects += asset.new_objects as u64;
            self.canonical_assets_seen += 1;
            self.canonical_assets_new += u64::from(asset.canonical_asset_new);
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportReport {
    pub disposition: CandidateDisposition,
    pub metrics: ImportMetrics,
}

impl ImportReport {
    pub fn imported(metrics: ImportMetrics) -> Self {
        Self {
            disposition: CandidateDisposition::Imported,
            metrics,
        }
    }

    pub fn skipped() -> Self {
        Self {
            disposition: CandidateDisposition::Skipped,
            metrics: ImportMetrics::default(),
        }
    }
}

pub fn path_disk_bytes(path: &Path) -> u64 {
    if path.is_file() {
        return metadata(path).map(|metadata| metadata.len()).unwrap_or(0);
    }
    if !path.is_dir() {
        return 0;
    }
    WalkDir::new(path)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| entry.metadata().ok())
        .filter(|metadata| metadata.is_file())
        .map(|metadata| metadata.len())
        .sum()
}

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}
