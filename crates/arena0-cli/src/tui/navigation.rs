#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkspaceView {
    Overview,
    Negotiation,
    Program,
    PublicTrace,
    Wasm,
    SystemEvents,
}

impl WorkspaceView {
    pub(super) fn from_number(number: char) -> Option<Self> {
        Some(match number {
            '1' => Self::Overview,
            '2' => Self::Negotiation,
            '3' => Self::Program,
            '4' => Self::PublicTrace,
            '5' => Self::Wasm,
            '6' => Self::SystemEvents,
            _ => return None,
        })
    }

    pub(super) const fn index(self) -> usize {
        match self {
            Self::Overview => 0,
            Self::Negotiation => 1,
            Self::Program => 2,
            Self::PublicTrace => 3,
            Self::Wasm => 4,
            Self::SystemEvents => 5,
        }
    }

    pub(super) const fn overview_pane(self) -> Option<OverviewPane> {
        match self {
            Self::Overview => None,
            Self::Negotiation => Some(OverviewPane::Negotiation),
            Self::Program => Some(OverviewPane::Program),
            Self::PublicTrace => Some(OverviewPane::PublicTrace),
            Self::Wasm => Some(OverviewPane::Wasm),
            Self::SystemEvents => Some(OverviewPane::SystemEvents),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OverviewPane {
    Negotiation,
    Program,
    PublicTrace,
    Wasm,
    SystemEvents,
}

impl OverviewPane {
    pub(super) const fn next(self) -> Self {
        match self {
            Self::Program => Self::PublicTrace,
            Self::PublicTrace => Self::Negotiation,
            Self::Negotiation => Self::Wasm,
            Self::Wasm => Self::SystemEvents,
            Self::SystemEvents => Self::Program,
        }
    }

    pub(super) const fn previous(self) -> Self {
        match self {
            Self::Program => Self::SystemEvents,
            Self::PublicTrace => Self::Program,
            Self::Negotiation => Self::PublicTrace,
            Self::Wasm => Self::Negotiation,
            Self::SystemEvents => Self::Wasm,
        }
    }

    pub(super) const fn view(self) -> WorkspaceView {
        match self {
            Self::Negotiation => WorkspaceView::Negotiation,
            Self::Program => WorkspaceView::Program,
            Self::PublicTrace => WorkspaceView::PublicTrace,
            Self::Wasm => WorkspaceView::Wasm,
            Self::SystemEvents => WorkspaceView::SystemEvents,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Focus {
    Hosts,
    Workspace,
    Composer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DetailRegion {
    Records,
    Inspector,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Page {
    Overview(OverviewPane),
    Detail {
        pane: OverviewPane,
        region: DetailRegion,
    },
}

impl Default for Page {
    fn default() -> Self {
        Self::Overview(OverviewPane::Program)
    }
}

impl Page {
    pub(super) fn view(self) -> WorkspaceView {
        match self {
            Self::Overview(_) => WorkspaceView::Overview,
            Self::Detail { pane, .. } => pane.view(),
        }
    }

    pub(super) fn pane(self) -> OverviewPane {
        match self {
            Self::Overview(pane) | Self::Detail { pane, .. } => pane,
        }
    }

    pub(super) fn region(self) -> DetailRegion {
        match self {
            Self::Overview(_) => DetailRegion::Records,
            Self::Detail { region, .. } => region,
        }
    }

    pub(super) fn inspector(self) -> Option<OverviewPane> {
        match self {
            Self::Detail {
                pane,
                region: DetailRegion::Inspector,
            } => Some(pane),
            _ => None,
        }
    }

    pub(super) fn select(&mut self, view: WorkspaceView) {
        *self = match view.overview_pane() {
            Some(pane) => Self::Detail {
                pane,
                region: DetailRegion::Records,
            },
            None => Self::Overview(self.pane()),
        };
    }

    pub(super) fn overview(&mut self) {
        *self = Self::Overview(self.pane());
    }

    pub(super) fn set_pane(&mut self, pane: OverviewPane) {
        *self = Self::Overview(pane);
    }

    pub(super) fn open_detail(&mut self) {
        *self = Self::Detail {
            pane: self.pane(),
            region: DetailRegion::Records,
        };
    }

    pub(super) fn open_inspector(&mut self) {
        if let Self::Detail { region, .. } = self {
            *region = DetailRegion::Inspector;
        }
    }

    pub(super) fn close_inspector(&mut self) {
        if let Self::Detail { region, .. } = self {
            *region = DetailRegion::Records;
        }
    }
}
