//! rsd-ingest: scan-based reconciliation, the event coalescer, and the work
//! applier (P1.3, P1.4). The rule that makes convergence tractable (DESIGN.md
//! §7.1): work items say *look here*, never *believe this* — `lstat` is the only
//! truth-resolver, so stale, duplicated, or coalesced events can never corrupt
//! the catalog, only cost a redundant probe.

pub mod coalesce;
pub mod scan;

pub use coalesce::{Coalescer, CoalescerConfig, IngestEvent};
pub use scan::{apply_work, bootstrap, rescan, ScanStats, WorkItem, WorkKind};

#[derive(Debug, thiserror::Error)]
pub enum IngestError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("catalog: {0}")]
    Catalog(#[from] rsd_catalog::CatalogError),
}

pub type Result<T> = std::result::Result<T, IngestError>;
