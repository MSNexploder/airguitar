/// Error returned by most functions.
///
/// Maybe consider a specialized error handling crate or defining an error
/// type as an `enum` of causes.
/// However, for our example, using a boxed `std::error::Error` is sufficient.
///
/// For performance reasons, boxing is avoided in any hot path. For example, in
/// `parse`, a custom error `enum` is defined. This is because the error is hit
/// and handled during normal execution when a partial message is received on a
/// socket. `std::error::Error` is implemented for `parse::Error` which allows
/// it to be converted to `Box<dyn std::error::Error>`.
pub(crate) type Error = Box<dyn std::error::Error + Send + Sync>;
