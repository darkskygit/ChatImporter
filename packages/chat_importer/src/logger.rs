use fern::Dispatch;
use indicatif::{MultiProgress, ProgressDrawTarget};
use indicatif_log_bridge::LogWrapper;
use log::{LevelFilter, Log, Metadata, Record};

pub fn init_logger(level: LevelFilter) -> Result<MultiProgress, log::SetLoggerError> {
    let progress = MultiProgress::new();
    progress.set_draw_target(ProgressDrawTarget::stderr_with_hz(6));
    let (max_level, logger) = Dispatch::new()
        .level(level)
        .level_for("lepton_jpeg", LevelFilter::Warn)
        .format(move |out, message, record| {
            out.finish(format_args!(
                "{}[{:>5}][{}] {}",
                chrono::Local::now().format("[%H:%M:%S]"),
                record.level(),
                record.target(),
                message
            ))
        })
        .chain(Box::new(Logger {}) as Box<dyn Log>)
        .into_log();
    LogWrapper::new(progress.clone(), logger).try_init()?;
    log::set_max_level(max_level);
    Ok(progress)
}

struct Logger;

impl Log for Logger {
    fn enabled(&self, _metadata: &Metadata) -> bool {
        true
    }

    fn log(&self, record: &Record) {
        if self.enabled(record.metadata()) {
            println!("{}", record.args());
        }
    }

    fn flush(&self) {}
}
