//! Pure package, import, and render core for SHR Sampler.

pub mod engine;
pub mod offline;
pub mod package;
pub mod schema;
pub mod sfz;

pub use engine::{Engine, EngineError, Event, TimedEvent};
pub use offline::{OfflineError, RenderEvent, RenderSpec, render, write_wav};
pub use package::{PreparedInstrument, load_package};
pub use schema::{
    Adsr, InstrumentManifest, InstrumentMetadata, LoopMode, PlaybackMode, SampleMetadata,
    SampleRef, ZoneManifest,
};
pub use sfz::{ImportMetadata, ImportReport, SfzError, import_sfz};
