mod matcher;

use anyhow::Result;
use gchdb::SqliteChatRecorder;
use matcher::qq_mht_importer;

fn main() -> Result<()> {
    let mut recorder = SqliteChatRecorder::new("record.db")?;
    qq_mht_importer(&mut recorder, "test.mht")?;
    Ok(())
}
