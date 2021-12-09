use crate::error::Error;

pub(crate) type Result<T> = std::result::Result<T, Error>;
