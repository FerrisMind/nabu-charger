//! Симулятор устройства: TCP-сервер, эмулирующий регистры зарядника SMB.
//!
//! Нужен, чтобы демонстрировать и проверять реальный транспорт (`TCP`) без
//! железа: `Simulator` поднимает сервер на `127.0.0.1`, а клиент подключается
//! [`TcpTransport`](crate::tcp::TcpTransport) с тем же протоколом, что и на стенде.
//!
//! # Пример
//!
//! ```no_run
//! use host::prelude::*;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let sim = Simulator::start(AdapterType::Hvdcp3)?;
//! let clock = SystemClock::start();
//! let transport = TcpTransport::connect(sim.addr(), std::time::Duration::from_millis(500))?;
//! let mut charger = Charger::open(transport, &clock, &NullJournal, ChargerConfig::for_nabu())?;
//! let outcome = run_until_ready(&mut charger, &clock, RunOptions::default(), |_| Ok(()))?;
//! assert_eq!(outcome.adapter, AdapterType::Hvdcp3);
//! sim.stop();
//! # Ok(())
//! # }
//! ```

use charger_core::testkit::{Fault, ScriptedMockTransport};
use charger_core::{AdapterType, ChargerTransport, regs};
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::{self, JoinHandle};

/// Команда симулятору.
#[derive(Debug, Clone, Copy)]
pub enum SimCommand {
    /// Сменить подключённый адаптер.
    SetAdapter(AdapterType),
    /// Задать значение регистра.
    SetReg(u16, u8),
    /// Внести сбой.
    InjectFault(Fault),
    /// Остановить сервер.
    Shutdown,
}

/// Пульт управления работающим симулятором.
#[derive(Debug, Clone)]
pub struct SimulatorHandle {
    sender: Sender<SimCommand>,
}

impl SimulatorHandle {
    /// Меняет тип подключённого адаптера.
    pub fn set_adapter(&self, adapter: AdapterType) {
        let _ = self.sender.send(SimCommand::SetAdapter(adapter));
    }

    /// Задаёт значение регистра.
    pub fn set_reg(&self, addr: u16, value: u8) {
        let _ = self.sender.send(SimCommand::SetReg(addr, value));
    }

    /// Вносит сбой в модель устройства.
    pub fn inject_fault(&self, fault: Fault) {
        let _ = self.sender.send(SimCommand::InjectFault(fault));
    }
}

/// Запущенный симулятор устройства.
#[derive(Debug)]
pub struct Simulator {
    addr: SocketAddr,
    handle: SimulatorHandle,
    join: Option<JoinHandle<()>>,
}

impl Simulator {
    /// Поднимает сервер на свободном порту localhost.
    ///
    /// # Errors
    ///
    /// [`std::io::Error`], если не удалось занять порт или запустить поток.
    pub fn start(adapter: AdapterType) -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        let (sender, receiver) = channel::<SimCommand>();
        let join = thread::Builder::new()
            .name("nabu-sim".to_owned())
            .spawn(move || serve(&listener, adapter, &receiver))?;
        Ok(Self {
            addr,
            handle: SimulatorHandle { sender },
            join: Some(join),
        })
    }

    /// Адрес, к которому подключается клиент.
    #[must_use]
    pub const fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Пульт управления.
    #[must_use]
    pub fn handle(&self) -> SimulatorHandle {
        self.handle.clone()
    }

    /// Останавливает сервер и дожидается завершения потока.
    pub fn stop(mut self) {
        let _ = self.handle.sender.send(SimCommand::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for Simulator {
    fn drop(&mut self) {
        let _ = self.handle.sender.send(SimCommand::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn serve(listener: &TcpListener, adapter: AdapterType, receiver: &Receiver<SimCommand>) {
    let mut device = ScriptedMockTransport::for_adapter(adapter);
    let _ = listener.set_nonblocking(true);
    loop {
        // Обрабатываем команды пульта.
        while let Ok(command) = receiver.try_recv() {
            match command {
                SimCommand::SetAdapter(next) => device.set_adapter(next),
                SimCommand::SetReg(addr, value) => device.set_reg(addr, value),
                SimCommand::InjectFault(fault) => device.push_fault(fault),
                SimCommand::Shutdown => return,
            }
        }
        match listener.accept() {
            Ok((stream, _)) => {
                let _ = stream.set_nonblocking(false);
                // `true` означает запрос остановки: завершаем поток сервера.
                if let Ok(true) = handle_client(stream, &mut device, receiver) {
                    return;
                }
            }
            Err(ref err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(_) => return,
        }
    }
}

/// Обслуживает одно соединение.
///
/// Возвращает `true`, если поступила команда остановки сервера: вызывающая
/// сторона должна завершить поток, иначе `join()` при остановке никогда не вернётся.
fn handle_client(
    stream: TcpStream,
    device: &mut ScriptedMockTransport,
    receiver: &Receiver<SimCommand>,
) -> std::io::Result<bool> {
    // Короткий таймаут чтения нужен, чтобы сервер замечал команду остановки,
    // не дожидаясь разрыва соединения клиентом.
    stream.set_read_timeout(Some(std::time::Duration::from_millis(50)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;
    let mut line = String::new();
    loop {
        while let Ok(command) = receiver.try_recv() {
            match command {
                SimCommand::SetAdapter(next) => device.set_adapter(next),
                SimCommand::SetReg(addr, value) => device.set_reg(addr, value),
                SimCommand::InjectFault(fault) => device.push_fault(fault),
                SimCommand::Shutdown => return Ok(true),
            }
        }
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => return Ok(false),
            Ok(_) => {
                let response = dispatch(device, line.trim());
                writer.write_all(response.as_bytes())?;
                writer.write_all(b"\n")?;
                writer.flush()?;
            }
            Err(err)
                if err.kind() == std::io::ErrorKind::WouldBlock
                    || err.kind() == std::io::ErrorKind::TimedOut =>
            {
                // Нет запроса: возвращаемся к проверке пульта управления.
            }
            Err(err) => return Err(err),
        }
    }
}

fn dispatch(device: &mut ScriptedMockTransport, request: &str) -> String {
    let mut parts = request.split_whitespace();
    match parts.next() {
        Some("R") => match parts.next().map(parse_addr) {
            Some(Ok(addr)) => match device.read(addr) {
                Ok(value) => format!("V {value:02X}"),
                Err(_) => "ERR 1".to_owned(),
            },
            _ => "ERR 2".to_owned(),
        },
        Some("W") => {
            let addr = parts.next().map(parse_addr);
            let value = parts.next().map(parse_addr);
            match (addr, value) {
                (Some(Ok(addr)), Some(Ok(value))) => {
                    match u8::try_from(value).ok().map(|v| device.write(addr, v)) {
                        Some(Ok(())) => "OK".to_owned(),
                        _ => "ERR 1".to_owned(),
                    }
                }
                _ => "ERR 2".to_owned(),
            }
        }
        Some("RESET") => match device.reset() {
            Ok(()) => "OK".to_owned(),
            Err(_) => "ERR 1".to_owned(),
        },
        _ => "ERR 3".to_owned(),
    }
}

fn parse_addr(text: &str) -> Result<u16, ()> {
    u16::from_str_radix(text, 16).map_err(|_| ())
}

/// Полезная проверка: симулятор отвечает теми же значениями, что и модель регистров.
#[must_use]
pub fn default_register_probe(adapter: AdapterType) -> (u8, u8) {
    let mut device = ScriptedMockTransport::for_adapter(adapter);
    let mut status = 0_u8;
    if device.read(regs::APSD_STATUS).is_ok() {
        status = device.reg(regs::APSD_STATUS);
    }
    (status, device.reg(regs::APSD_RESULT_STATUS))
}
