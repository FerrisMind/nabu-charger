//! Real TCP transport: a hardware bench, an emulator or a remote lab.
//!
//! The protocol is deliberately simple and textual, easy to reproduce on either side:
//!
//! | Request | Response | Meaning |
//! |---|---|---|
//! | `R <addr:04X>` | `V <value:02X>` | register read |
//! | `W <addr:04X> <value:02X>` | `OK` | register write |
//! | `RESET` | `OK` | channel reset |
//! | any | `ERR <code>` | device failure |
//!
//! Lines are terminated with `\n`. Addresses and values are hexadecimal without a prefix.

use charger_core::{ChargerTransport, TransportError, TransportErrorKind};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// Transport to the device over TCP.
#[derive(Debug)]
pub struct TcpTransport {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
    command: String,
}

impl TcpTransport {
    /// Connects to the device.
    ///
    /// # Errors
    ///
    /// [`TransportError`] with kind [`TransportErrorKind::Io`] if the address is
    /// unreachable or the connection is not established within `timeout`.
    pub fn connect(addr: impl ToSocketAddrs, timeout: Duration) -> Result<Self, TransportError> {
        let writer = TcpStream::connect(addr)
            .map_err(|_| TransportError::io("failed to connect to the device"))?;
        writer
            .set_read_timeout(Some(timeout))
            .map_err(|_| TransportError::io("failed to set the read timeout"))?;
        writer
            .set_write_timeout(Some(timeout))
            .map_err(|_| TransportError::io("failed to set the write timeout"))?;
        let reader = BufReader::new(
            writer
                .try_clone()
                .map_err(|_| TransportError::io("failed to clone the stream"))?,
        );
        Ok(Self {
            reader,
            writer,
            command: String::with_capacity(32),
        })
    }

    fn transact(&mut self, request: &str) -> Result<String, TransportError> {
        self.writer
            .write_all(request.as_bytes())
            .and_then(|()| self.writer.write_all(b"\n"))
            .and_then(|()| self.writer.flush())
            .map_err(|_| classify_write_error())?;

        self.command.clear();
        let read = self
            .reader
            .read_line(&mut self.command)
            .map_err(|_| classify_read_error())?;
        if read == 0 {
            return Err(TransportError::disconnected(
                "the device closed the connection",
            ));
        }
        Ok(self.command.trim().to_owned())
    }
}

fn classify_read_error() -> TransportError {
    TransportError::new(
        TransportErrorKind::Timeout,
        std::io::ErrorKind::TimedOut as i32,
        "no response from the device",
    )
}

fn classify_write_error() -> TransportError {
    TransportError::new(
        TransportErrorKind::Io,
        std::io::ErrorKind::BrokenPipe as i32,
        "failed to send the request",
    )
}

fn parse_hex(text: &str) -> Result<u16, TransportError> {
    u16::from_str_radix(text, 16).map_err(|_| TransportError::protocol("invalid device response"))
}

impl ChargerTransport for TcpTransport {
    fn read(&mut self, addr: u16) -> Result<u8, TransportError> {
        let request = format!("R {addr:04X}");
        let response = self.transact(&request)?;
        let mut parts = response.split(' ');
        match (parts.next(), parts.next()) {
            (Some("V"), Some(value)) => {
                let parsed = parse_hex(value)?;
                u8::try_from(parsed)
                    .map_err(|_| TransportError::protocol("value out of byte range"))
            }
            (Some("ERR"), _) => Err(TransportError::io("the device returned a read error")),
            _ => Err(TransportError::protocol("unexpected response to a read")),
        }
    }

    fn write(&mut self, addr: u16, value: u8) -> Result<(), TransportError> {
        let request = format!("W {addr:04X} {value:02X}");
        let response = self.transact(&request)?;
        match response.as_str() {
            "OK" => Ok(()),
            other if other.starts_with("ERR") => {
                Err(TransportError::io("the device returned a write error"))
            }
            _ => Err(TransportError::protocol("unexpected response to a write")),
        }
    }

    fn reset(&mut self) -> Result<(), TransportError> {
        let response = self.transact("RESET")?;
        match response.as_str() {
            "OK" => Ok(()),
            _ => Err(TransportError::protocol(
                "the device did not acknowledge the reset",
            )),
        }
    }

    fn name(&self) -> &'static str {
        "tcp"
    }
}
