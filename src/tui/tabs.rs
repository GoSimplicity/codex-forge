#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FocusMode {
    Browse,
    Compose,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DetailTab {
    Messages,
    Node,
    Approvals,
    Artifacts,
    Events,
}

impl DetailTab {
    pub(crate) fn all() -> [Self; 5] {
        [
            Self::Messages,
            Self::Node,
            Self::Approvals,
            Self::Artifacts,
            Self::Events,
        ]
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Messages => "消息",
            Self::Node => "节点",
            Self::Approvals => "审批",
            Self::Artifacts => "产物",
            Self::Events => "事件",
        }
    }

    pub(crate) fn next(self) -> Self {
        match self {
            Self::Messages => Self::Node,
            Self::Node => Self::Approvals,
            Self::Approvals => Self::Artifacts,
            Self::Artifacts => Self::Events,
            Self::Events => Self::Messages,
        }
    }

    pub(crate) fn index(self) -> usize {
        match self {
            Self::Messages => 0,
            Self::Node => 1,
            Self::Approvals => 2,
            Self::Artifacts => 3,
            Self::Events => 4,
        }
    }
}
