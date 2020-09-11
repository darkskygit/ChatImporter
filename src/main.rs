mod args;
mod logger;
mod matcher;

use anyhow::Result;
use args::{get_cmd, get_owner, get_paths, SubCommand};
use gchdb::SqliteChatRecorder;
use logger::init_logger;
use matcher::{exporter, info, ExportType};

fn main() -> Result<()> {
    init_logger()?;
    let mut recorder = SqliteChatRecorder::new("record.db")?;
    for path in get_paths() {
        info!("Processing: {}", path.display());
        exporter(
            &mut recorder,
            match get_cmd() {
                SubCommand::QQ { .. } => ExportType::WindowsQQ(path, get_owner()),
                SubCommand::WeChat { .. } => ExportType::iOSWeChat(path),
                SubCommand::SMS { .. } => ExportType::iOSSMS(path, get_owner()),
            },
        )?;
    }
    Ok(())
}
