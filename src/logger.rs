use fern::Dispatch;
use log::{LevelFilter, Log, Metadata, Record};

pub fn init_logger(level: LevelFilter) -> Result<(), log::SetLoggerError> {
    Dispatch::new()
        .level(level)
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
        .apply()
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
