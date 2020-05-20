use path_absolutize::Absolutize;
use std::io::{Error, ErrorKind};
use std::path::PathBuf;
use structopt::StructOpt;
use walkdir::WalkDir;

#[derive(StructOpt)]
struct Args {
    #[structopt(short = "o", default_value = "DarkSky")]
    owner: String,
    #[structopt(name = "DIR", parse(try_from_str = check_path))]
    path: Vec<PathBuf>,
}

impl Args {
    fn get_files(&self) -> Vec<PathBuf> {
        self.path
            .iter()
            .flat_map(|path| {
                WalkDir::new(path)
                    .into_iter()
                    .filter_map(|e| e.map(|item| item.into_path()).ok())
            })
            .filter(|p| p.is_file())
            .collect()
    }
}

fn check_path<S: ToString>(src: S) -> Result<PathBuf, Error> {
    let path = PathBuf::from(src.to_string()).absolutize()?;
    if !path.exists() {
        Err(Error::new(
            ErrorKind::NotFound,
            format!("路径不存在: {}", path.display()),
        ))
    } else {
        Ok(path)
    }
}

pub fn get_files() -> Vec<PathBuf> {
    Args::from_args().get_files()
}

pub fn get_owner() -> String {
    Args::from_args().owner
}
