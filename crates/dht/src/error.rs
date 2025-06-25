use std::time::Duration;

use bencode::SerializeError;

#[derive(thiserror::Error, Debug)]
#[error("error looking up {hostname}: {err:#}")]
pub struct LookupError {
    hostname: Box<str>,
    #[source]
    err: std::io::Error,
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("error binding UDP socket: {0:#}")]
    Bind(#[source] Box<librqbit_dualstack_sockets::Error>),

    #[error("bootstrapping failed")]
    BootstrapFailed,

    #[error("{0} failed: {1:?}")]
    TaskFailed(&'static &'static str, #[source] Box<Error>),

    #[error("{0} finished unexpectedly with no error")]
    TaskQuit(&'static &'static str),

    #[error("no successful lookups, {errors} errors")]
    NoSuccessfulLookups { errors: usize },

    #[error("dht is dead")]
    DhtDead,

    #[error("receiver is dead")]
    ReceiverDead,

    #[error("error response")]
    ErrorResponse,

    #[error("timeout at {0:?}")]
    ResponseTimeout(Duration),

    #[error("bad transaction id")]
    BadTransactionId,

    #[error("outstanding request not found")]
    RequestNotFound,

    #[error(transparent)]
    BootstrapLookup(Box<LookupError>),

    #[error("error sending: {0:#}")]
    Send(#[source] std::io::Error),
    #[error("error in recv: {0:#}")]
    Recv(#[source] std::io::Error),

    #[error("bencode serialize error: {0:#}")]
    Serialize(#[source] Box<SerializeError>),
}

impl From<SerializeError> for Error {
    fn from(value: SerializeError) -> Self {
        Error::Serialize(Box::new(value))
    }
}

impl Error {
    pub fn lookup(hostname: &str, err: std::io::Error) -> Error {
        Error::BootstrapLookup(Box::new(LookupError {
            hostname: hostname.into(),
            err,
        }))
    }

    pub fn task_finished(name: &'static &'static str, result: Result<()>) -> Result<()> {
        match result {
            Ok(()) => Err(Error::TaskQuit(name)),
            Err(e) => Err(Error::TaskFailed(name, Box::new(e))),
        }
    }
}

pub type Result<T> = core::result::Result<T, Error>;
