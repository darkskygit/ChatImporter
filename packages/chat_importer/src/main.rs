mod args;
mod logger;
mod matcher;
mod store;

use anyhow::Result;
use args::{get_cmd, get_log_level, get_paths, SubCommand};
use logger::init_logger;
use matcher::{exporter, info, ExportType};
use store::ChatStore;

#[tokio::main]
async fn main() -> Result<()> {
    init_logger(get_log_level().to_level_filter())?;
    let mut store = ChatStore::open("record.db").await?;
    for path in get_paths() {
        info!("Processing: {}", path.display());
        exporter(
            &mut store,
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
        )
        .await?;
    }
    Ok(())
}
