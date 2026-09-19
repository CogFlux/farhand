use std::fmt;

pub type Result<T> = std::result::Result<T, Error>;

/// Every failure FarHand reports to a model or a user. `Denied` is the one
/// that matters most: it means the secret guard or the local allowlist
/// refused an operation, and the reason is safe to show.
#[derive(Debug)]
pub enum Error {
    /// No configuration exists anywhere in the resolution order. Not a
    /// broken config: for agents this simply means "not active here".
    NoConfig,
    Config(String),
    Denied(String),
    Ssh(String),
    Io(std::io::Error),
    Invalid(String),
    Timeout {
        secs: u64,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NoConfig => write!(
                f,
                "no config found: create .farhand.toml in the project (`farhand init > \
                 .farhand.toml`), or pass --config, set FARHAND_CONFIG, or write \
                 ~/.config/farhand/config.toml"
            ),
            Error::Config(m) => write!(f, "config: {m}"),
            Error::Denied(m) => write!(f, "denied: {m}"),
            Error::Ssh(m) => write!(f, "ssh: {m}"),
            Error::Io(e) => write!(f, "io: {e}"),
            Error::Invalid(m) => write!(f, "invalid: {m}"),
            Error::Timeout { secs } => write!(f, "timed out after {secs}s"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<openssh::Error> for Error {
    fn from(e: openssh::Error) -> Self {
        Error::Ssh(e.to_string())
    }
}

impl From<openssh_sftp_client::Error> for Error {
    fn from(e: openssh_sftp_client::Error) -> Self {
        Error::Ssh(format!("sftp: {e}"))
    }
}

impl Error {
    pub fn kind(&self) -> &'static str {
        match self {
            Error::NoConfig => "no-config",
            Error::Config(_) => "config",
            Error::Denied(_) => "denied",
            Error::Ssh(_) => "ssh",
            Error::Io(_) => "io",
            Error::Invalid(_) => "invalid",
            Error::Timeout { .. } => "timeout",
        }
    }
}
