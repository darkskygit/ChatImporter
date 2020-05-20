mod matcher;

use anyhow::{Context, Result};
use gchdb::{ChatRecoder, Record, RecordType, SqliteChatRecorder};
use log::{error, warn};
use matcher::{MsgMatcher, QQMhtMsgMatcher};
use std::fs::read;
use std::path::Path;

fn main() -> Result<()> {
    let mut recorder = SqliteChatRecorder::new("record.db")?;
    if let Some(records) = QQMhtMsgMatcher::new(&read("test.mht")?, "test".into())?.get_records() {
        for record in records {
            let content = record
                .get_record()
                .map(|r| r.content.clone())
                .unwrap_or_default();
            if !recorder.insert_or_update_record(record)? {
                warn!("Failed to insert record: {}", content);
            }
        }
    }
    recorder.refresh_index()?;
    Ok(())
}
