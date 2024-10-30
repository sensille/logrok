use clog_macros::CLog;
use clog::prelude::*;

#[derive(CLog, Clone, Copy, Debug)]
#[clog(
    logmod = LogModKind::Application,
    with_log,
)]

pub enum LogKeys {
    #[clog(name = "main")]
    MA,
    #[clog(name = "search")]
    SE,
    #[clog(name = "split")]
    SP,
    #[clog(name = "cache")]
    CA,
    #[clog(name = "lines")]
    LI,
}
