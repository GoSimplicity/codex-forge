use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ThinkingMode {
    Quick,
    #[default]
    Balanced,
    HardThink,
}

impl ThinkingMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Quick => "quick",
            Self::Balanced => "balanced",
            Self::HardThink => "hard-think",
        }
    }

    pub fn codex_reasoning_effort(self) -> &'static str {
        match self {
            Self::Quick => "low",
            Self::Balanced => "medium",
            Self::HardThink => "high",
        }
    }
}
