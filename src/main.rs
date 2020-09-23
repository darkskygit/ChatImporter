#![feature(bool_to_option)]

mod args;
mod logger;
mod matcher;

use anyhow::Result;
use args::{get_cmd, get_paths, SubCommand};
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
                SubCommand::QQ { owner, .. } => ExportType::WindowsQQ(path, owner.into()),
                SubCommand::WeChat { chat_names, .. } => ExportType::iOSWeChat(
                    path,
                    chat_names.as_ref().map(|names| {
                        (!names.is_empty())
                            .then_some(names.split(',').map(|s| s.into()).collect())
                            .unwrap_or_default()
                    }),
                ),
                SubCommand::SMS { owner, .. } => ExportType::iOSSMS(path, owner.into()),
            },
        )?;
    }
    Ok(())
}
