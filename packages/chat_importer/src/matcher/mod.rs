mod ios_sms;
mod ios_wc;
mod utils;
mod win_qq_html;
mod win_qq_mht;

use base64::{engine::general_purpose::STANDARD, Engine};
use htmlescape::decode_html;
use lazy_static::lazy_static;
pub use log::{debug, error, info, warn};
use path_ext::PathExt;
use regex::{Captures, Regex};
use utils::{blob_dhash, hamming_distance};

use crate::store::{Attachments, ChatStore, MetadataMerger, Record, RecordType};
use assetpack_core::Hash32;

pub trait MsgMatcher {
    fn get_records(&self) -> Option<Vec<RecordType>>;
    fn get_metadata_merger(&self) -> Option<Box<dyn MetadataMerger>> {
        None
    }
}

use anyhow::{Context, Result};
use std::fs::read;
use std::path::Path;
use std::time::Instant;

#[allow(non_camel_case_types)]
pub enum ExportType<P: AsRef<Path>> {
    WindowsQQ(P, String),
    iOSWeChat(P, Option<Vec<String>>),
    iOSSMS(P, String),
}

pub async fn exporter<P>(store: &mut ChatStore, export_type: ExportType<P>) -> Result<()>
where
    P: AsRef<Path>,
{
    let matcher = match export_type {
        ExportType::WindowsQQ(path, owner) => win_qq_mht::Matcher::new(
            &read(&path)?,
            owner,
            path.as_ref()
                .file_stem()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .into(),
        )?,
        ExportType::iOSWeChat(path, names) => ios_wc::Matcher::new(path, names)?,
        ExportType::iOSSMS(path, owner) => ios_sms::Matcher::new(path, owner)?,
    };
    export_matcher(store, matcher.as_ref()).await
}

async fn export_matcher(store: &mut ChatStore, matcher: &dyn MsgMatcher) -> Result<()> {
    let records = matcher.get_records().context("Cannot transfrom records")?;
    let total = records.len();
    let merger = matcher.get_metadata_merger();
    let mut progress = 0.0;
    let mut sw = Instant::now();
    for (i, record) in records.into_iter().enumerate() {
        if (i + 1) as f64 / total as f64 - progress > 0.01 {
            progress = (i + 1) as f64 / total as f64;
            info!(
                "current progress: {:.2}%, {}/{}, {}ms",
                progress * 100.0,
                i,
                total,
                sw.elapsed().as_millis()
            );
            sw = Instant::now();
        }
        let display = record.display();
        if !store
            .insert_or_update(record, merger.as_deref())
            .await
            .context(format!("Cannot insert records: {}", display))?
        {
            warn!("Failed to insert record: {}", display);
        }
    }
    Ok(())
}

fn gen_md5<S: ToString>(user_name: S) -> String {
    use md5::{Digest, Md5};
    format!("{:x}", Md5::digest(user_name.to_string().as_bytes()))
}

fn hex2b64(hex: &str) -> String {
    hex::decode(&hex)
        .map(|h| STANDARD.encode(&h))
        .unwrap_or_else(|_| hex.into())
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
