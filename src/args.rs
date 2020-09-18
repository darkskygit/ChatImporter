use lazy_static::*;
use path_absolutize::Absolutize;
use path_ext::PathExt;
use std::io::{Error, ErrorKind};
use std::path::PathBuf;
use structopt::StructOpt;
use walkdir::WalkDir;

#[derive(StructOpt)]
pub enum SubCommand {
    #[structopt(name = "qq", about = "import qq mht files")]
    QQ {
        #[structopt(short = "o", default_value = "DarkSky")]
        owner: String,
        #[structopt(name = "DIR", parse(try_from_str = check_path))]
        path: Vec<PathBuf>,
    },
    #[structopt(name = "wc", about = "import wechat from ios backup")]
    WeChat {
        #[structopt(short = "c")]
        chat_names: Option<Vec<String>>,
        #[structopt(name = "DIR", parse(try_from_str = check_path))]
        path: Vec<PathBuf>,
    },
    #[structopt(name = "sms", about = "import sms from ios backup")]
    SMS {
        #[structopt(short = "o", default_value = "DarkSky")]
        owner: String,
        #[structopt(name = "DIR", parse(try_from_str = check_path))]
        path: Vec<PathBuf>,
    },
}

#[derive(StructOpt)]
struct Args {
    #[structopt(subcommand)]
    cmd: SubCommand,
}

impl Args {
    fn get_paths(&self) -> Vec<PathBuf> {
        match &self.cmd {
            SubCommand::QQ { path, .. } => path
                .iter()
                .flat_map(|path| {
                    WalkDir::new(path)
                        .into_iter()
                        .filter_map(|e| e.map(|item| item.into_path()).ok())
                })
                .filter(|p| p.is_file())
                .collect(),
            SubCommand::WeChat { path, .. } | SubCommand::SMS { path, .. } => path
                .iter()
                .map(PathBuf::from)
                .filter(PathBuf::is_dir)
                .collect(),
        }
    }
}

fn check_path<S: AsRef<str>>(src: S) -> Result<PathBuf, Error> {
    let path = PathBuf::from(src.as_ref());
    let path = path.absolutize()?;
    if !path.exists() {
        Err(Error::new(
            ErrorKind::NotFound,
            format!("路径不存在: {}", path.display()),
        ))
    } else {
        Ok(path.into())
    }
}

lazy_static! {
    static ref ARGS: Args = Args::from_args();
}

pub fn get_paths() -> Vec<PathBuf> {
    ARGS.get_paths()
}

pub fn get_cmd() -> &'static SubCommand {
    &ARGS.cmd
}
