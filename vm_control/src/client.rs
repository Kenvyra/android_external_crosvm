// Copyright 2021 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use crate::*;
use base::{info, net::UnixSeqpacket, validate_raw_descriptor, RawDescriptor, Tube};

use std::fs::OpenOptions;
use std::num::ParseIntError;
use std::path::{Path, PathBuf};

enum ModifyBatError {
    BatControlErr(BatControlResult),
}

impl fmt::Display for ModifyBatError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::ModifyBatError::*;

        match self {
            BatControlErr(e) => write!(f, "{}", e),
        }
    }
}

pub enum ModifyUsbError {
    ArgMissing(&'static str),
    ArgParse(&'static str, String),
    ArgParseInt(&'static str, String, ParseIntError),
    FailedDescriptorValidate(base::Error),
    PathDoesNotExist(PathBuf),
    SocketFailed,
    UnexpectedResponse(VmResponse),
    UnknownCommand(String),
    UsbControl(UsbControlResult),
}

impl std::fmt::Display for ModifyUsbError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        use self::ModifyUsbError::*;

        match self {
            ArgMissing(a) => write!(f, "argument missing: {}", a),
            ArgParse(name, value) => {
                write!(f, "failed to parse argument {} value `{}`", name, value)
            }
            ArgParseInt(name, value, e) => write!(
                f,
                "failed to parse integer argument {} value `{}`: {}",
                name, value, e
            ),
            FailedDescriptorValidate(e) => write!(f, "failed to validate file descriptor: {}", e),
            PathDoesNotExist(p) => write!(f, "path `{}` does not exist", p.display()),
            SocketFailed => write!(f, "socket failed"),
            UnexpectedResponse(r) => write!(f, "unexpected response: {}", r),
            UnknownCommand(c) => write!(f, "unknown command: `{}`", c),
            UsbControl(e) => write!(f, "{}", e),
        }
    }
}

pub type ModifyUsbResult<T> = std::result::Result<T, ModifyUsbError>;

fn raw_descriptor_from_path(path: &Path) -> ModifyUsbResult<RawDescriptor> {
    if !path.exists() {
        return Err(ModifyUsbError::PathDoesNotExist(path.to_owned()));
    }
    let raw_descriptor = path
        .file_name()
        .and_then(|fd_osstr| fd_osstr.to_str())
        .map_or(
            Err(ModifyUsbError::ArgParse(
                "USB_DEVICE_PATH",
                path.to_string_lossy().into_owned(),
            )),
            |fd_str| {
                fd_str.parse::<libc::c_int>().map_err(|e| {
                    ModifyUsbError::ArgParseInt("USB_DEVICE_PATH", fd_str.to_owned(), e)
                })
            },
        )?;
    validate_raw_descriptor(raw_descriptor).map_err(ModifyUsbError::FailedDescriptorValidate)
}

pub type VmsRequestResult = std::result::Result<(), ()>;

pub fn vms_request(request: &VmRequest, socket_path: &Path) -> VmsRequestResult {
    let response = handle_request(request, socket_path)?;
    info!("request response was {}", response);
    Ok(())
}

pub fn do_usb_attach(
    socket_path: &Path,
    bus: u8,
    addr: u8,
    vid: u16,
    pid: u16,
    dev_path: &Path,
) -> ModifyUsbResult<UsbControlResult> {
    let usb_file: File = if dev_path.parent() == Some(Path::new("/proc/self/fd")) {
        // Special case '/proc/self/fd/*' paths. The FD is already open, just use it.
        // Safe because we will validate |raw_fd|.
        unsafe { File::from_raw_descriptor(raw_descriptor_from_path(&dev_path)?) }
    } else {
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(&dev_path)
            .map_err(|_| ModifyUsbError::UsbControl(UsbControlResult::FailedToOpenDevice))?
    };

    let request = VmRequest::UsbCommand(UsbControlCommand::AttachDevice {
        bus,
        addr,
        vid,
        pid,
        file: usb_file,
    });
    let response =
        handle_request(&request, socket_path).map_err(|_| ModifyUsbError::SocketFailed)?;
    match response {
        VmResponse::UsbResponse(usb_resp) => Ok(usb_resp),
        r => Err(ModifyUsbError::UnexpectedResponse(r)),
    }
}

pub fn do_usb_detach(socket_path: &Path, port: u8) -> ModifyUsbResult<UsbControlResult> {
    let request = VmRequest::UsbCommand(UsbControlCommand::DetachDevice { port });
    let response =
        handle_request(&request, socket_path).map_err(|_| ModifyUsbError::SocketFailed)?;
    match response {
        VmResponse::UsbResponse(usb_resp) => Ok(usb_resp),
        r => Err(ModifyUsbError::UnexpectedResponse(r)),
    }
}

pub fn do_usb_list(socket_path: &Path) -> ModifyUsbResult<UsbControlResult> {
    let mut ports: [u8; USB_CONTROL_MAX_PORTS] = Default::default();
    for (index, port) in ports.iter_mut().enumerate() {
        *port = index as u8
    }
    let request = VmRequest::UsbCommand(UsbControlCommand::ListDevice { ports });
    let response =
        handle_request(&request, socket_path).map_err(|_| ModifyUsbError::SocketFailed)?;
    match response {
        VmResponse::UsbResponse(usb_resp) => Ok(usb_resp),
        r => Err(ModifyUsbError::UnexpectedResponse(r)),
    }
}

pub type DoModifyBatteryResult = std::result::Result<(), ()>;

pub fn do_modify_battery(
    socket_path: &Path,
    battery_type: &str,
    property: &str,
    target: &str,
) -> DoModifyBatteryResult {
    let response = match battery_type.parse::<BatteryType>() {
        Ok(type_) => match BatControlCommand::new(property.to_string(), target.to_string()) {
            Ok(cmd) => {
                let request = VmRequest::BatCommand(type_, cmd);
                Ok(handle_request(&request, socket_path)?)
            }
            Err(e) => Err(ModifyBatError::BatControlErr(e)),
        },
        Err(e) => Err(ModifyBatError::BatControlErr(e)),
    };

    match response {
        Ok(response) => {
            println!("{}", response);
            Ok(())
        }
        Err(e) => {
            println!("error {}", e);
            Err(())
        }
    }
}

pub type HandleRequestResult = std::result::Result<VmResponse, ()>;

pub fn handle_request(request: &VmRequest, socket_path: &Path) -> HandleRequestResult {
    match UnixSeqpacket::connect(&socket_path) {
        Ok(s) => {
            let socket = Tube::new(s);
            if let Err(e) = socket.send(request) {
                error!(
                    "failed to send request to socket at '{:?}': {}",
                    socket_path, e
                );
                return Err(());
            }
            match socket.recv() {
                Ok(response) => Ok(response),
                Err(e) => {
                    error!(
                        "failed to send request to socket at '{:?}': {}",
                        socket_path, e
                    );
                    Err(())
                }
            }
        }
        Err(e) => {
            error!("failed to connect to socket at '{:?}': {}", socket_path, e);
            Err(())
        }
    }
}
