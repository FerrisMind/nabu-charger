//! Device simulator: a TCP server emulating the registers of the SMB charger.
//!
//! Needed to demonstrate and verify the real transport (`TCP`) without hardware:
//! `Simulator` starts a server on `127.0.0.1`, and the client connects with
//! [`TcpTransport`](crate::tcp::TcpTransport) using the same protocol as on the bench.
//!
//! # Example
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

/// Command to the simulator.
#[derive(Debug, Clone, Copy)]
pub enum SimCommand {
    /// Change the connected adapter.
    SetAdapter(AdapterType),
    /// Set a register value.
    SetReg(u16, u8),
    /// Inject a fault.
    InjectFault(Fault),
    /// Stop the server.
    Shutdown,
}

/// Control handle for a running simulator.
#[derive(Debug, Clone)]
pub struct SimulatorHandle {
    sender: Sender<SimCommand>,
}

impl SimulatorHandle {
    /// Changes the type of the connected adapter.
    pub fn set_adapter(&self, adapter: AdapterType) {
        let _ = self.sender.send(SimCommand::SetAdapter(adapter));
    }

    /// Sets a register value.
    pub fn set_reg(&self, addr: u16, value: u8) {
        let _ = self.sender.send(SimCommand::SetReg(addr, value));
    }

    /// Injects a fault into the device model.
    pub fn inject_fault(&self, fault: Fault) {
        let _ = self.sender.send(SimCommand::InjectFault(fault));
    }
}

/// A running device simulator.
#[derive(Debug)]
pub struct Simulator {
    addr: SocketAddr,
    handle: SimulatorHandle,
    join: Option<JoinHandle<()>>,
}

impl Simulator {
    /// Starts the server on a free localhost port.
    ///
    /// # Errors
    ///
    /// [`std::io::Error`] if the port could not be bound or the thread did not start.
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

    /// The address the client connects to.
    #[must_use]
    pub const fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Control handle.
    #[must_use]
    pub fn handle(&self) -> SimulatorHandle {
        self.handle.clone()
    }

    /// Stops the server and waits for the thread to finish.
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
        // Process control commands.
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
                // `true` means a stop request: we finish the server thread.
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

/// Serves one connection.
///
/// Returns `true` if a server stop command arrived: the caller must finish the
/// thread, otherwise `join()` on stop would never return.
fn handle_client(
    stream: TcpStream,
    device: &mut ScriptedMockTransport,
    receiver: &Receiver<SimCommand>,
) -> std::io::Result<bool> {
    // A short read timeout is needed so the server notices the stop command
    // without waiting for the client to drop the connection.
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
                // No request: go back to checking the control channel.
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

/// A useful check: the simulator answers with the same values as the register model.
#[must_use]
pub fn default_register_probe(adapter: AdapterType) -> (u8, u8) {
    let mut device = ScriptedMockTransport::for_adapter(adapter);
    let mut status = 0_u8;
    if device.read(regs::APSD_STATUS).is_ok() {
        status = device.reg(regs::APSD_STATUS);
    }
    (status, device.reg(regs::APSD_RESULT_STATUS))
}
