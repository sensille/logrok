use clog::prelude::*;
use std::fmt;

#[derive(Clone, Copy, Debug)]
#[repr(u32)]
pub enum LogKeys {
    MA,
    SE,
    SP,
    CA,
    LI,
}

pub static LOG_KEYS: &'static [LogKeys] =
    &[MA, SE, SP, CA, LI];

use LogKeys::*;
impl LogKey for LogKeys {
    fn log_key(&self) -> ContextKey { *self as usize }
}
impl fmt::Display for LogKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::MA => write!(f, "main"),
            Self::SE => write!(f, "search"),
            Self::SP => write!(f, "split"),
            Self::CA => write!(f, "cache"),
            Self::LI => write!(f, "lines"),
        }
    }
}
