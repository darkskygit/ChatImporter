mod args;
mod logger;
mod matcher;

use anyhow::Result;
use args::{get_files, get_owner};
use gchdb::SqliteChatRecorder;
use logger::init_logger;
use matcher::{info, qq_mht_importer};

fn main() -> Result<()> {
    init_logger()?;
    let mut recorder = SqliteChatRecorder::new("record.db")?;
    for file in get_files() {
        info!("Processing: {}", file.display());
        qq_mht_importer(&mut recorder, file, get_owner())?;
    }
    Ok(())
}
