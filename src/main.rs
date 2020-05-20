mod matcher;

use anyhow::{Context, Result};
use gchdb::{Attachment, ChatRecoder, Record, SqliteChatRecorder};
use log::{error, warn};
use matcher::{MsgMatcher, QQMsgMatcher, QQPathAttachGetter};
use std::fs::read_to_string;
use std::path::Path;

fn main() -> Result<()> {
    let mut recorder = SqliteChatRecorder::new("record.db")?;
    for record in transfrom_chat_to_records("test.html")? {
        if !recorder.insert_or_update_record(&record)? {
            warn!("Failed to insert record: {}", record.content);
        }
    }
    recorder.refresh_index()?;
    Ok(())
}

fn transfrom_chat_to_records<P: AsRef<Path>>(path: P) -> Result<Vec<Record>> {
    QQMsgMatcher::new(read_to_string(path)?, "test".into(), QQPathAttachGetter)
        .get_records()
        .context("Cannot transfrom records")
}
