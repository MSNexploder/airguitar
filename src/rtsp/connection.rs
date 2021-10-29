use crate::Result;
use bytes::{Buf, BytesMut};
use rtsp_types::{Message, ParseError, Response};
use std::{fmt::Debug, net::SocketAddr};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, BufWriter},
    net::TcpStream,
};
use tracing::trace;

/// Send and receive `Message` values from a remote peer.
///
/// When implementing networking protocols, a message on that protocol is
/// often composed of several smaller messages known as messages. The purpose of
/// `Connection` is to read and write messages on the underlying `TcpStream`.
///
/// To read messages, the `Connection` uses an internal buffer, which is filled
/// up until there are enough bytes to create a full message. Once this happens,
/// the `Connection` creates the message and returns it to the caller.
///
/// When sending messages, the message is first encoded into the write buffer.
/// The contents of the write buffer are then written to the socket.
#[derive(Debug)]
pub(crate) struct Connection {
    // The `TcpStream`. It is decorated with a `BufWriter`, which provides write
    // level buffering. The `BufWriter` implementation provided by Tokio is
    // sufficient for our needs.
    stream: BufWriter<TcpStream>,

    // The buffer for reading messages.
    buffer: BytesMut,

    pub(crate) local_addr: SocketAddr,
    pub(crate) peer_addr: SocketAddr,
}

impl Connection {
    /// Create a new `Connection`, backed by `socket`. Read and write buffers
    /// are initialized.
    pub fn new(socket: TcpStream) -> Result<Connection> {
        let local_addr = socket.local_addr()?;
        let peer_addr = socket.peer_addr()?;

        Ok(Connection {
            stream: BufWriter::new(socket),
            // Default to a 4KB read buffer.
            buffer: BytesMut::with_capacity(4 * 1024),

            local_addr: local_addr,
            peer_addr: peer_addr,
        })
    }

    /// Read a single `Message` value from the underlying stream.
    ///
    /// The function waits until it has retrieved enough data to parse a message.
    /// Any data remaining in the read buffer after the message has been parsed is
    /// kept there for the next call to `read_message`.
    ///
    /// # Returns
    ///
    /// On success, the received message is returned. If the `TcpStream`
    /// is closed in a way that doesn't break a message in half, it returns
    /// `None`. Otherwise, an error is returned.
    pub async fn read_message(&mut self) -> Result<Option<Message<Vec<u8>>>> {
        loop {
            // Attempt to parse a message from the buffered data. If enough data
            // has been buffered, the message is returned.
            if let Some(message) = self.parse_message()? {
                return Ok(Some(message));
            }

            // There is not enough buffered data to read a message. Attempt to
            // read more data from the socket.
            //
            // On success, the number of bytes is returned. `0` indicates "end
            // of stream".
            if 0 == self.stream.read_buf(&mut self.buffer).await? {
                // The remote closed the connection. For this to be a clean
                // shutdown, there should be no data in the read buffer. If
                // there is, this means that the peer closed the socket while
                // sending a message.
                if self.buffer.is_empty() {
                    return Ok(None);
                } else {
                    return Err("connection reset by peer".into());
                }
            }
        }
    }

    /// Tries to parse a message from the buffer. If the buffer contains enough
    /// data, the message is returned and the data removed from the buffer. If not
    /// enough data has been buffered yet, `Ok(None)` is returned. If the
    /// buffered data does not represent a valid message, `Err` is returned.
    fn parse_message(&mut self) -> Result<Option<Message<Vec<u8>>>> {
        match Message::parse(&self.buffer[..]) {
            Ok((message, consumed)) => {
                // Discard the parsed data from the read buffer.
                self.buffer.advance(consumed);

                // Return the parsed message to the caller.
                Ok(Some(message))
            }
            // There is not enough data present in the read buffer to parse a
            // single message. We must wait for more data to be received from the
            // socket. Reading from the socket will be done in the statement
            // after this `match`.
            //
            // We do not want to return `Err` from here as this "error" is an
            // expected runtime condition.
            Err(ParseError::Incomplete) => Ok(None),
            // An error was encountered while parsing the message. The connection
            // is now in an invalid state. Returning `Err` from here will result
            // in the connection being closed.
            Err(e) => Err(e.into()),
        }
    }

    /// Write a single `Response` value to the underlying stream.
    ///
    /// The `Response` value is written to the socket using the various `write_*`
    /// functions provided by `AsyncWrite`. Calling these functions directly on
    /// a `TcpStream` is **not** advised, as this will result in a large number of
    /// syscalls. However, it is fine to call these functions on a *buffered*
    /// write stream. The data will be written to the buffer. Once the buffer is
    /// full, it is flushed to the underlying socket.
    pub async fn write_response<B: AsRef<[u8]> + Debug>(
        &mut self,
        response: &Response<B>,
    ) -> crate::Result<()> {
        trace!("{:?}", response);

        let mut buffer = Vec::new();
        response.write(&mut buffer)?;
        self.stream.write_all(&buffer).await?;

        // Ensure the encoded message is written to the socket. The calls above
        // are to the buffered stream and writes. Calling `flush` writes the
        // remaining contents of the buffer to the socket.
        self.stream.flush().await?;
        Ok(())
    }
}
