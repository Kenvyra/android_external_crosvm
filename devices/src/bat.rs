// Copyright 2020 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::{BusAccessInfo, BusDevice};
use acpi_tables::{aml, aml::Aml};
use base::{error, warn, AsRawDescriptor, Descriptor, Event, PollContext, PollToken};
use msg_socket::{MsgReceiver, MsgSender};
use std::fmt::{self, Display};
use std::os::unix::io::RawFd;
use std::sync::Arc;
use std::thread;
use sync::Mutex;
use vm_control::{BatControlCommand, BatControlResponseSocket, BatControlResult};

/// Errors for battery devices.
#[derive(Debug)]
pub enum BatteryError {
    Non32BitMmioAddress,
}

impl Display for BatteryError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::BatteryError::*;

        match self {
            Non32BitMmioAddress => write!(f, "Non 32-bit mmio address space"),
        }
    }
}

type Result<T> = std::result::Result<T, BatteryError>;

/// the GoldFish Battery MMIO length.
pub const GOLDFISHBAT_MMIO_LEN: u64 = 0x1000;

struct GoldfishBatteryState {
    // interrupt state
    int_status: u32,
    int_enable: u32,
    // AC state
    ac_online: u32,
    // Battery state
    status: u32,
    health: u32,
    present: u32,
    capacity: u32,
}

macro_rules! create_battery_func {
    // $property: the battery property which is going to be modified.
    // $int: the interrupt status which is going to be set to notify the guest.
    ($fn:ident, $property:ident, $int:ident) => {
        fn $fn(&mut self, value: u32) -> bool {
            let old = std::mem::replace(&mut self.$property, value);
            old != self.$property && self.set_int_status($int)
        }
    };
}

impl GoldfishBatteryState {
    fn set_int_status(&mut self, mask: u32) -> bool {
        if ((self.int_enable & mask) != 0) && ((self.int_status & mask) == 0) {
            self.int_status |= mask;
            return true;
        }
        false
    }

    fn int_status(&self) -> u32 {
        self.int_status
    }

    create_battery_func!(set_ac_online, ac_online, AC_STATUS_CHANGED);

    create_battery_func!(set_status, status, BATTERY_STATUS_CHANGED);

    create_battery_func!(set_health, health, BATTERY_STATUS_CHANGED);

    create_battery_func!(set_present, present, BATTERY_STATUS_CHANGED);

    create_battery_func!(set_capacity, capacity, BATTERY_STATUS_CHANGED);
}

/// GoldFish Battery state
pub struct GoldfishBattery {
    state: Arc<Mutex<GoldfishBatteryState>>,
    mmio_base: u32,
    irq_num: u32,
    irq_evt: Event,
    irq_resample_evt: Event,
    activated: bool,
    monitor_thread: Option<thread::JoinHandle<()>>,
    kill_evt: Option<Event>,
    socket: Option<BatControlResponseSocket>,
}

/// Goldfish Battery MMIO offset
const BATTERY_INT_STATUS: u32 = 0;
const BATTERY_INT_ENABLE: u32 = 0x4;
const BATTERY_AC_ONLINE: u32 = 0x8;
const BATTERY_STATUS: u32 = 0xC;
const BATTERY_HEALTH: u32 = 0x10;
const BATTERY_PRESENT: u32 = 0x14;
const BATTERY_CAPACITY: u32 = 0x18;
const BATTERY_VOLTAGE: u32 = 0x1C;
const BATTERY_TEMP: u32 = 0x20;
const BATTERY_CHARGE_COUNTER: u32 = 0x24;
const BATTERY_VOLTAGE_MAX: u32 = 0x28;
const BATTERY_CURRENT_MAX: u32 = 0x2C;
const BATTERY_CURRENT_NOW: u32 = 0x30;
const BATTERY_CURRENT_AVG: u32 = 0x34;
const BATTERY_CHARGE_FULL_UAH: u32 = 0x38;
const BATTERY_CYCLE_COUNT: u32 = 0x40;

/// Goldfish Battery interrupt bits
const BATTERY_STATUS_CHANGED: u32 = 1 << 0;
const AC_STATUS_CHANGED: u32 = 1 << 1;
const BATTERY_INT_MASK: u32 = BATTERY_STATUS_CHANGED | AC_STATUS_CHANGED;

fn command_monitor(
    socket: BatControlResponseSocket,
    irq_evt: Event,
    irq_resample_evt: Event,
    kill_evt: Event,
    state: Arc<Mutex<GoldfishBatteryState>>,
) {
    #[derive(PollToken)]
    enum Token {
        Commands,
        Resample,
        Kill,
    }

    let poll_ctx: PollContext<Token> = match PollContext::build_with(&[
        (&Descriptor(socket.as_raw_descriptor()), Token::Commands),
        (
            &Descriptor(irq_resample_evt.as_raw_descriptor()),
            Token::Resample,
        ),
        (&Descriptor(kill_evt.as_raw_descriptor()), Token::Kill),
    ]) {
        Ok(pc) => pc,
        Err(e) => {
            error!("failed to build PollContext: {}", e);
            return;
        }
    };

    'poll: loop {
        let events = match poll_ctx.wait() {
            Ok(v) => v,
            Err(e) => {
                error!("error while polling for events: {}", e);
                break;
            }
        };

        for event in events.iter_readable() {
            match event.token() {
                Token::Commands => {
                    let req = match socket.recv() {
                        Ok(req) => req,
                        Err(e) => {
                            error!("failed to receive request: {}", e);
                            continue;
                        }
                    };

                    let mut bat_state = state.lock();
                    let inject_irq = match req {
                        BatControlCommand::SetStatus(status) => bat_state.set_status(status.into()),
                        BatControlCommand::SetHealth(health) => bat_state.set_health(health.into()),
                        BatControlCommand::SetPresent(present) => {
                            let v = if present != 0 { 1 } else { 0 };
                            bat_state.set_present(v)
                        }
                        BatControlCommand::SetCapacity(capacity) => {
                            let v = std::cmp::min(capacity, 100);
                            bat_state.set_capacity(v)
                        }
                        BatControlCommand::SetACOnline(ac_online) => {
                            let v = if ac_online != 0 { 1 } else { 0 };
                            bat_state.set_ac_online(v)
                        }
                    };

                    if inject_irq {
                        let _ = irq_evt.write(1);
                    }

                    if let Err(e) = socket.send(&BatControlResult::Ok) {
                        error!("failed to send response: {}", e);
                    }
                }
                Token::Resample => {
                    let _ = irq_resample_evt.read();
                    if state.lock().int_status() != 0 {
                        let _ = irq_evt.write(1);
                    }
                }
                Token::Kill => break 'poll,
            }
        }
    }
}

impl GoldfishBattery {
    /// Create GoldfishBattery device model
    ///
    /// * `mmio_base` - The 32-bit mmio base address.
    /// * `irq_num` - The corresponding interrupt number of the irq_evt
    ///               which will be put into the ACPI DSDT.
    /// * `irq_evt` - The interrupt event used to notify driver about
    ///               the battery properties changing.
    /// * `irq_resample_evt` - Resample interrupt event notified at EOI.
    /// * `socket` - Battery control socket
    pub fn new(
        mmio_base: u64,
        irq_num: u32,
        irq_evt: Event,
        irq_resample_evt: Event,
        socket: BatControlResponseSocket,
    ) -> Result<Self> {
        if mmio_base + GOLDFISHBAT_MMIO_LEN - 1 > u32::MAX as u64 {
            return Err(BatteryError::Non32BitMmioAddress);
        }
        let state = Arc::new(Mutex::new(GoldfishBatteryState {
            capacity: 50,
            health: 1,
            present: 1,
            status: 1,
            ac_online: 1,
            int_enable: 0,
            int_status: 0,
        }));

        Ok(GoldfishBattery {
            state,
            mmio_base: mmio_base as u32,
            irq_num,
            irq_evt,
            irq_resample_evt,
            activated: false,
            monitor_thread: None,
            kill_evt: None,
            socket: Some(socket),
        })
    }

    /// return the fds used by this device
    pub fn keep_fds(&self) -> Vec<RawFd> {
        let mut fds = vec![
            self.irq_evt.as_raw_descriptor(),
            self.irq_resample_evt.as_raw_descriptor(),
        ];

        if let Some(socket) = &self.socket {
            fds.push(socket.as_raw_descriptor());
        }

        fds
    }

    /// start a monitor thread to monitor the events from host
    fn start_monitor(&mut self) {
        if self.activated {
            return;
        }

        let (self_kill_evt, kill_evt) = match Event::new().and_then(|e| Ok((e.try_clone()?, e))) {
            Ok(v) => v,
            Err(e) => {
                error!(
                    "{}: failed to create kill EventFd pair: {}",
                    self.debug_label(),
                    e
                );
                return;
            }
        };

        if let Some(socket) = self.socket.take() {
            let irq_evt = self.irq_evt.try_clone().unwrap();
            let irq_resample_evt = self.irq_resample_evt.try_clone().unwrap();
            let bat_state = self.state.clone();
            let monitor_result = thread::Builder::new()
                .name(self.debug_label())
                .spawn(move || {
                    command_monitor(socket, irq_evt, irq_resample_evt, kill_evt, bat_state);
                });

            self.monitor_thread = match monitor_result {
                Err(e) => {
                    error!(
                        "{}: failed to spawn PowerIO monitor: {}",
                        self.debug_label(),
                        e
                    );
                    return;
                }
                Ok(join_handle) => Some(join_handle),
            };
            self.kill_evt = Some(self_kill_evt);
            self.activated = true;
        }
    }
}

impl Drop for GoldfishBattery {
    fn drop(&mut self) {
        if let Some(kill_evt) = self.kill_evt.take() {
            // Ignore the result because there is nothing we can do with a failure.
            let _ = kill_evt.write(1);
        }
        if let Some(thread) = self.monitor_thread.take() {
            let _ = thread.join();
        }
    }
}

impl BusDevice for GoldfishBattery {
    fn debug_label(&self) -> String {
        "GoldfishBattery".to_owned()
    }

    fn read(&mut self, info: BusAccessInfo, data: &mut [u8]) {
        if data.len() != std::mem::size_of::<u32>() {
            warn!(
                "{}: unsupported read length {}, only support 4bytes read",
                self.debug_label(),
                data.len()
            );
            return;
        }

        let val = match info.offset as u32 {
            BATTERY_INT_STATUS => {
                // read to clear the interrupt status
                std::mem::replace(&mut self.state.lock().int_status, 0)
            }
            BATTERY_INT_ENABLE => self.state.lock().int_enable,
            BATTERY_AC_ONLINE => self.state.lock().ac_online,
            BATTERY_STATUS => self.state.lock().status,
            BATTERY_HEALTH => self.state.lock().health,
            BATTERY_PRESENT => self.state.lock().present,
            BATTERY_CAPACITY => self.state.lock().capacity,
            BATTERY_VOLTAGE => 0,
            BATTERY_TEMP => 0,
            BATTERY_CHARGE_COUNTER => 0,
            BATTERY_VOLTAGE_MAX => 0,
            BATTERY_CURRENT_MAX => 0,
            BATTERY_CURRENT_NOW => 0,
            BATTERY_CURRENT_AVG => 0,
            BATTERY_CHARGE_FULL_UAH => 0,
            BATTERY_CYCLE_COUNT => 0,
            _ => {
                warn!("{}: unsupported read address {}", self.debug_label(), info);
                return;
            }
        };

        let val_arr = val.to_ne_bytes();
        data.copy_from_slice(&val_arr);
    }

    fn write(&mut self, info: BusAccessInfo, data: &[u8]) {
        if data.len() != std::mem::size_of::<u32>() {
            warn!(
                "{}: unsupported write length {}, only support 4bytes write",
                self.debug_label(),
                data.len()
            );
            return;
        }

        let mut val_arr = u32::to_ne_bytes(0 as u32);
        val_arr.copy_from_slice(data);
        let val = u32::from_ne_bytes(val_arr);

        match info.offset as u32 {
            BATTERY_INT_ENABLE => {
                self.state.lock().int_enable = val;
                if (val & BATTERY_INT_MASK) != 0 && !self.activated {
                    self.start_monitor();
                }
            }
            _ => {
                warn!("{}: Bad write to address {}", self.debug_label(), info);
            }
        };
    }
}

impl Aml for GoldfishBattery {
    fn to_aml_bytes(&self, bytes: &mut Vec<u8>) {
        aml::Device::new(
            "GFBY".into(),
            vec![
                &aml::Name::new("_HID".into(), &"GFSH0001"),
                &aml::Name::new(
                    "_CRS".into(),
                    &aml::ResourceTemplate::new(vec![
                        &aml::Memory32Fixed::new(true, self.mmio_base, GOLDFISHBAT_MMIO_LEN as u32),
                        &aml::Interrupt::new(true, false, false, true, self.irq_num),
                    ]),
                ),
            ],
        )
        .to_aml_bytes(bytes);
    }
}