mod args;
mod logger;
mod matcher;

use anyhow::Result;
use args::{get_cmd, get_log_level, get_paths, SubCommand};
use gchdb::SqliteChatRecorder;
use logger::init_logger;
use matcher::{exporter, info, ExportType};

fn main() -> Result<()> {
    init_logger(get_log_level().to_level_filter())?;
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

#[test]
fn test_load_blobs() {
    use rusqlite::{Connection, OpenFlags};
    use serde::Deserialize;
    use serde_json::from_slice;
    use std::fs::{create_dir_all, read, write};
    use std::path::PathBuf;

    #[derive(Deserialize)]
    struct OldNew {
        old: String,
        new: String,
    }
    let old_path = PathBuf::from("export/old");
    let new_path = PathBuf::from("export/new");
    create_dir_all(&old_path).unwrap();
    create_dir_all(&new_path).unwrap();
    let data = read("1.json").unwrap();
    let array: Vec<OldNew> = from_slice(&data).unwrap();
    let conn = Connection::open_with_flags(
        "record.db",
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .unwrap();
    let mut command = conn
        .prepare(r#"SELECT blob FROM blobs where hash = $1"#)
        .unwrap();
    for (idx, item) in array.iter().enumerate() {
        let old: Vec<u8> = command
            .query_map([item.old.clone()], |row| row.get(0))
            .unwrap()
            .next()
            .unwrap()
            .unwrap();
        write(
            old_path.clone().join(format!("{},{}.png", idx, &item.old)),
            old,
        )
        .unwrap();
        let new: Vec<u8> = command
            .query_map([item.new.clone()], |row| row.get(0))
            .unwrap()
            .next()
            .unwrap()
            .unwrap();
        write(
            new_path.clone().join(format!("{},{}.png", idx, &item.new)),
            new,
        )
        .unwrap();
    }
}
