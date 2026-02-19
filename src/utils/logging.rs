use log::{Log, Metadata, Record, SetLoggerError};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

pub struct RingBufferLogger {
    buffer: Arc<Mutex<VecDeque<String>>>,
    inner: env_logger::Logger,
    capacity: usize,
}

impl RingBufferLogger {
    pub fn new(capacity: usize) -> Self {
        let inner = env_logger::Builder::from_default_env().build();
        Self {
            buffer: Arc::new(Mutex::new(VecDeque::with_capacity(capacity))),
            inner,
            capacity,
        }
    }

    pub fn get_buffer_handle(&self) -> Arc<Mutex<VecDeque<String>>> {
        self.buffer.clone()
    }

    pub fn init_globally(self) -> Result<(), SetLoggerError> {
        let max_level = self.inner.filter();
        log::set_boxed_logger(Box::new(self))?;
        log::set_max_level(max_level);
        Ok(())
    }
}

pub fn read_recent_logs(buffer: &Arc<Mutex<VecDeque<String>>>, limit: usize) -> Vec<String> {
    let Ok(logs) = buffer.lock() else {
        return vec!["[ERROR] Failed to read log buffer".to_string()];
    };

    let start = logs.len().saturating_sub(limit);
    logs.iter().skip(start).cloned().collect()
}

impl Log for RingBufferLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        self.inner.enabled(metadata)
    }

    fn log(&self, record: &Record) {
        // Always capture info+ level logs to buffer for GUI debug window
        let should_buffer = record.level() <= log::Level::Info;

        if self.enabled(record.metadata()) || should_buffer {
            let msg = format!("[{}] {}", record.level(), record.args());

            // Console output via env_logger (respects RUST_LOG)
            if self.enabled(record.metadata()) {
                self.inner.log(record);
            }

            // Buffer output (always captures info+)
            if should_buffer {
                if let Ok(mut buffer) = self.buffer.lock() {
                    if buffer.len() >= self.capacity {
                        buffer.pop_front();
                    }
                    buffer.push_back(msg);
                }
            }
        }
    }

    fn flush(&self) {
        self.inner.flush();
    }
}
