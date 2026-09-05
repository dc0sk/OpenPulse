use thiserror::Error;

#[derive(Debug, Error)]
pub enum PttError {
    #[error("serial port error: {0}")]
    Serial(String),
    #[error("rigctld connection error: {0}")]
    Rigctld(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// The configured backend cannot be honoured AT ALL — an unknown name, a feature that is not
    /// compiled in, or a missing required device path.
    ///
    /// Distinct from `Serial`/`Rigctld`/`Io`, which mean "the backend is real and the attempt
    /// failed". The distinction is the whole point: a config error is a typo the operator must fix
    /// and the daemon refuses to start on it (#1285), while a connect failure may be a rig that is
    /// simply not powered up yet. Collapsing both into `None` is what let a mistyped `ptt_backend`
    /// start a daemon that then transmitted into an unkeyed rig.
    #[error("PTT configuration error: {0}")]
    Config(String),
}

/// Error type for full rig CAT control operations.
#[derive(Debug, Error)]
pub enum RadioError {
    #[error("rigctld I/O error: {0}")]
    RigctldIo(#[from] std::io::Error),
    #[error("rigctld protocol error: {0}")]
    RigctldProtocol(String),
    #[error("parse error: {0}")]
    Parse(String),
    /// The rig definition does not include the requested operation.
    #[error("operation not supported by this rig: {0}")]
    Unsupported(&'static str),
    /// Generic serial CAT error (I/O, protocol, or template expansion failure).
    #[error("generic CAT error: {0}")]
    GenericCat(String),
}
