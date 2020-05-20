mod matcher;

use anyhow::{Context, Result};
use gchdb::{ChatRecoder, SqliteChatRecorder};
use log::{error, warn};
use matcher::{MsgMatcher, QQMhtMsgMatcher};
use std::fs::read;

fn main() -> Result<()> {
    let mut recorder = SqliteChatRecorder::new("record.db")?;
    for record in QQMhtMsgMatcher::new(&read("test.mht")?, "test".into())?
        .get_records()
        .context("Cannot transfrom records")?
    {
        let content = record
            .get_record()
            .map(|r| r.content.clone())
            .unwrap_or_default();
        if !recorder.insert_or_update_record(record)? {
            warn!("Failed to insert record: {}", content);
        }
    }
    recorder.refresh_index()?;
    Ok(())
}
