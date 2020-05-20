mod matcher;

use anyhow::{Context, Result};
use gchdb::{ChatRecoder, SqliteChatRecorder};
use matcher::{warn, MsgMatcher, QQMhtMsgMatcher};
use std::fs::read;
use std::time::Instant;

fn main() -> Result<()> {
    let mut recorder = SqliteChatRecorder::new("record.db")?;
    let matcher = QQMhtMsgMatcher::new(&read("test.mht")?, "test".into())?;
    let records = matcher.get_records().context("Cannot transfrom records")?;
    let mut progress = 0.0;
    let mut sw = Instant::now();
    for (i, record) in records.iter().enumerate() {
        let content = record
            .get_record()
            .map(|r| r.content.clone())
            .unwrap_or_default();
        if (i + 1) as f64 / records.len() as f64 - progress > 0.01 {
            progress = (i + 1) as f64 / records.len() as f64;
            println!(
                "current progress: {}%, {}/{}, {}ms",
                progress,
                i,
                records.len(),
                sw.elapsed().as_millis()
            );
            sw = Instant::now();
        }
        if !recorder.insert_or_update_record(record.clone())? {
            warn!("Failed to insert record: {}", content);
        }
    }
    recorder.refresh_index()?;
    Ok(())
}
