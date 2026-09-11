//! PTT and full CAT rig controller traits and implementations for OpenPulseHF.
pub mod band_levels;
pub mod cat_controller;
pub mod cm108;
pub mod error;
pub mod generic_cat;
pub mod gpio;
pub mod noop;
pub mod ptt_builder;
pub mod rig_controller;
pub mod rig_definition;
pub mod rig_mode;
pub mod rigctld;
pub mod serial;
pub mod shared_ptt;
pub mod vox;

pub use band_levels::{attenuation_for_hz, freq_hz_to_band};
pub use cat_controller::CatController;
pub use cm108::Cm108Ptt;
pub use error::{PttError, RadioError};
pub use generic_cat::{GenericSerialCat, MockTransport, RigTransport};
pub use gpio::GpioPtt;
pub use noop::NoOpPtt;
pub use rig_controller::RigctldController;
pub use rig_definition::RigDefinition;
pub use rig_mode::RigMode;
pub use rigctld::RigctldPtt;
pub use shared_ptt::{PttKeyGuard, PttObserver, SharedPtt, UnkeyOutcome, DEFAULT_PTT_MAX};
pub use vox::VoxPtt;

/// Controls transmitter PTT (push-to-talk) state.
pub trait PttController {
    fn assert_ptt(&mut self) -> Result<(), PttError>;
    fn release_ptt(&mut self) -> Result<(), PttError>;
    fn is_asserted(&self) -> bool;
}
