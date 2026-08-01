//! Sidecar 生命周期管理。
//!
//! 本模块实现 JSONL stdio 协议的解析（[`protocol`]）和 Sidecar 进程
//! 的监督控制（[`supervisor`]）。协议定义见 `docs/protocol/v1/`。

pub mod collector;
pub mod protocol;
pub mod supervisor;

pub use collector::{CollectionRequest, CollectionStatus, CollectionSummary, run_collection};
pub use protocol::{
    CommandEnvelope, CommandType, EventEnvelope, EventType, MAX_MESSAGE_BYTES, PROTOCOL_VERSION,
};
pub use supervisor::{Sidecar, SidecarError, SpawnOptions, collection_spawn_options};
