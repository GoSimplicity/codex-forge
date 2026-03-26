#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FocusMode {
    Browse,
    Compose,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BrowsePane {
    Threads,
    Runs,
    Steps,
    Error,
    Detail,
    Composer,
}

impl BrowsePane {
    pub(crate) fn next(self) -> Self {
        match self {
            Self::Threads => Self::Runs,
            Self::Runs => Self::Steps,
            Self::Steps => Self::Error,
            Self::Error => Self::Detail,
            Self::Detail => Self::Composer,
            Self::Composer => Self::Threads,
        }
    }

    pub(crate) fn prev(self) -> Self {
        match self {
            Self::Threads => Self::Composer,
            Self::Runs => Self::Threads,
            Self::Steps => Self::Runs,
            Self::Error => Self::Steps,
            Self::Detail => Self::Error,
            Self::Composer => Self::Detail,
        }
    }
}
