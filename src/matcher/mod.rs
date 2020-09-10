mod ios_sms;
mod ios_wechat;
mod qq_html;
mod qq_mht;

use gchdb::{Blob, Record, RecordType};
use htmlescape::decode_html;
use lazy_static::lazy_static;
use path_ext::PathExt;
use regex::{Captures, Regex};

use qq_html::{QQAttachGetter, QQMsgImage};

pub use log::{error, info, warn};
pub use qq_html::{QQMsgMatcher, QQPathAttachGetter};
pub use qq_mht::QQMhtMsgMatcher;

pub trait MsgMatcher {
    fn get_records(&self) -> Option<Vec<RecordType>>;
}

use anyhow::{Context, Result};
use gchdb::{ChatRecoder, SqliteChatRecorder};
use std::fs::read;
use std::path::Path;
use std::time::Instant;

pub fn qq_mht_importer<P: AsRef<Path>>(
    recorder: &mut SqliteChatRecorder,
    path: P,
    owner: String,
) -> Result<()> {
    let filename = path
        .as_ref()
        .file_stem()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .into();
    let matcher = QQMhtMsgMatcher::new(&read(path)?, owner, filename)?;
    let records = matcher.get_records().context("Cannot transfrom records")?;
    let mut progress = 0.0;
    let mut sw = Instant::now();
    for (i, record) in records.iter().enumerate() {
        if (i + 1) as f64 / records.len() as f64 - progress > 0.01 {
            progress = (i + 1) as f64 / records.len() as f64;
            info!(
                "current progress: {:.2}%, {}/{}, {}ms",
                progress * 100.0,
                i,
                records.len(),
                sw.elapsed().as_millis()
            );
            sw = Instant::now();
        }
        if !recorder.insert_or_update_record(record.clone())? {
            let content = record
                .get_record()
                .map(|r| r.content.clone())
                .unwrap_or_default();
            warn!("Failed to insert record: {}", content);
        }
    }
    recorder.refresh_index()?;
    Ok(())
}
