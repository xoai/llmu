pub mod claude_code;
pub mod opencode;

use crate::types::UsageEvent;

/// Shared local collection result (FR-25): normalized usage events plus
/// bounded, secret-free diagnostic notes; callers merge fail-soft.
pub struct Collected {
    pub events: Vec<UsageEvent>,
    pub notes: Vec<String>,
}
