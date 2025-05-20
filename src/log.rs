use hclog_macros::HCLog;
use hclog::ScopeKey;

#[derive(HCLog, Clone, Copy, Debug)]
#[hclog(
    scope = ScopeKey::Application,
    with_log,
)]
pub enum LogKeys {
    #[hclog(name = "main")]
    MA,
    #[hclog(name = "search")]
    SE,
    #[hclog(name = "split")]
    SP,
    #[hclog(name = "cache")]
    CA,
    #[hclog(name = "lines")]
    LI,
}
