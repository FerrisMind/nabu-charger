//! Реальный транспорт по TCP: стенд с железом, эмулятор или удалённая лаборатория.
//!
//! Протокол намеренно простой и текстовый — его легко повторить на любой стороне:
//!
//! | Запрос | Ответ | Смысл |
//! |---|---|---|
//! | `R <addr:04X>` | `V <value:02X>` | чтение регистра |
//! | `W <addr:04X> <value:02X>` | `OK` | запись регистра |
//! | `RESET` | `OK` | сброс канала |
//! | любой | `ERR <code>` | отказ устройства |
//!
//! Строки завершаются `\n`. Адреса и значения — в шестнадцатеричном виде без префикса.

use charger_core::{ChargerTransport, TransportError, TransportErrorKind};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// Транспорт к устройству по TCP.
#[derive(Debug)]
pub struct TcpTransport {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
    command: String,
}

impl TcpTransport {
    /// Подключается к устройству.
    ///
    /// # Errors
    ///
    /// [`TransportError`] с категорией [`TransportErrorKind::Io`], если адрес
    /// недоступен или соединение не устанавливается за `timeout`.
    pub fn connect(addr: impl ToSocketAddrs, timeout: Duration) -> Result<Self, TransportError> {
        let writer = TcpStream::connect(addr)
            .map_err(|_| TransportError::io("не удалось подключиться к устройству"))?;
        writer
            .set_read_timeout(Some(timeout))
            .map_err(|_| TransportError::io("не удалось задать таймаут чтения"))?;
        writer
            .set_write_timeout(Some(timeout))
            .map_err(|_| TransportError::io("не удалось задать таймаут записи"))?;
        let reader = BufReader::new(
            writer
                .try_clone()
                .map_err(|_| TransportError::io("не удалось разделить поток"))?,
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
                "устройство закрыло соединение",
            ));
        }
        Ok(self.command.trim().to_owned())
    }
}

fn classify_read_error() -> TransportError {
    TransportError::new(
        TransportErrorKind::Timeout,
        std::io::ErrorKind::TimedOut as i32,
        "нет ответа от устройства",
    )
}

fn classify_write_error() -> TransportError {
    TransportError::new(
        TransportErrorKind::Io,
        std::io::ErrorKind::BrokenPipe as i32,
        "не удалось отправить запрос",
    )
}

fn parse_hex(text: &str) -> Result<u16, TransportError> {
    u16::from_str_radix(text, 16).map_err(|_| TransportError::protocol("неверный ответ устройства"))
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
                    .map_err(|_| TransportError::protocol("значение вне диапазона байта"))
            }
            (Some("ERR"), _) => Err(TransportError::io("устройство вернуло ошибку чтения")),
            _ => Err(TransportError::protocol("неожиданный ответ на чтение")),
        }
    }

    fn write(&mut self, addr: u16, value: u8) -> Result<(), TransportError> {
        let request = format!("W {addr:04X} {value:02X}");
        let response = self.transact(&request)?;
        match response.as_str() {
            "OK" => Ok(()),
            other if other.starts_with("ERR") => {
                Err(TransportError::io("устройство вернуло ошибку записи"))
            }
            _ => Err(TransportError::protocol("неожиданный ответ на запись")),
        }
    }

    fn reset(&mut self) -> Result<(), TransportError> {
        let response = self.transact("RESET")?;
        match response.as_str() {
            "OK" => Ok(()),
            _ => Err(TransportError::protocol("устройство не подтвердило сброс")),
        }
    }

    fn name(&self) -> &'static str {
        "tcp"
    }
}
