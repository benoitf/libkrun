// Minimal errno module for Windows, mirroring vmm_sys_util::errno interface.

use std::fmt::{Display, Formatter};
use std::io;
use std::result;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Error(i32);

pub type Result<T> = result::Result<T, Error>;

impl Error {
    pub fn new(errno: i32) -> Error {
        Error(errno)
    }

    pub fn last() -> Error {
        Error(io::Error::last_os_error().raw_os_error().unwrap())
    }

    pub fn errno(self) -> i32 {
        self.0
    }
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        io::Error::from_raw_os_error(self.0).fmt(f)
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::new(e.raw_os_error().unwrap_or_default())
    }
}

impl From<Error> for io::Error {
    fn from(err: Error) -> io::Error {
        io::Error::from_raw_os_error(err.0)
    }
}

pub fn errno_result<T>() -> Result<T> {
    Err(Error::last())
}
