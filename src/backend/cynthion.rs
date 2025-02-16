use std::collections::VecDeque;
use std::thread::{spawn, JoinHandle};
use std::time::Duration;
use std::sync::mpsc;

use anyhow::{Context as ErrorContext, Error, bail};
use futures_channel::oneshot;
use futures_lite::future::block_on;
use futures_util::{select_biased, FutureExt};
use num_enum::{FromPrimitive, IntoPrimitive};
use nusb::{
    self,
    transfer::{
        Control,
        ControlType,
        Recipient,
        RequestBuffer,
        TransferError,
    },
    DeviceInfo,
    Interface
};

const VID: u16 = 0x1d50;
const PID: u16 = 0x615b;

const CLASS: u8 = 0xff;
const SUBCLASS: u8 = 0x10;
const PROTOCOL: u8 = 0x00;

const ENDPOINT: u8 = 0x81;

const READ_LEN: usize = 0x4000;
const NUM_TRANSFERS: usize = 4;

#[derive(Copy, Clone, FromPrimitive, IntoPrimitive)]
#[repr(u8)]
pub enum Speed {
    #[default]
    High = 0,
    Full = 1,
    Low  = 2,
    Auto = 3,
}

impl Speed {
    pub fn description(&self) -> &'static str {
        use Speed::*;
        match self {
            Auto => "Auto",
            High => "High (480Mbps)",
            Full => "Full (12Mbps)",
            Low => "Low (1.5Mbps)",
        }
    }

    pub fn mask(&self) -> u8 {
        use Speed::*;
        match self {
            Auto => 0b0001,
            Low  => 0b0010,
            Full => 0b0100,
            High => 0b1000,
        }
    }
}

bitfield! {
    #[derive(Copy, Clone)]
    struct State(u8);
    bool, enable, set_enable: 0;
    u8, from into Speed, speed, set_speed: 2, 1;
}

impl State {
    fn new(enable: bool, speed: Speed) -> State {
        let mut state = State(0);
        state.set_enable(enable);
        state.set_speed(speed);
        state
    }
}

pub struct InterfaceSelection {
    interface_number: u8,
    alt_setting_number: u8,
}

/// Whether a Cynthion device is ready for use as an analyzer.
pub enum CynthionUsability {
    /// Device is usable via the given interface, at supported speeds.
    Usable(InterfaceSelection, Vec<Speed>),
    /// Device not usable, with a string explaining why.
    Unusable(String),
}

use CynthionUsability::*;

/// A Cynthion device attached to the system.
pub struct CynthionDevice {
    pub device_info: DeviceInfo,
    pub usability: CynthionUsability,
}

/// A handle to an open Cynthion device.
pub struct CynthionHandle {
    interface: Interface,
}

pub struct CynthionStream {
    receiver: mpsc::Receiver<Vec<u8>>,
    buffer: VecDeque<u8>,
}

pub struct CynthionStop {
    stop_request: oneshot::Sender<()>,
    worker: JoinHandle::<()>,
}

/// Check whether a Cynthion device has an accessible analyzer interface.
fn check_device(device_info: &DeviceInfo)
    -> Result<(InterfaceSelection, Vec<Speed>), Error>
{
    // Check we can open the device.
    let device = device_info
        .open()
        .context("Failed to open device")?;

    // Read the active configuration.
    let config = device
        .active_configuration()
        .context("Failed to retrieve active configuration")?;

    // Iterate over the interfaces...
    for interface in config.interfaces() {
        let interface_number = interface.interface_number();

        // ...and alternate settings...
        for alt_setting in interface.alt_settings() {
            let alt_setting_number = alt_setting.alternate_setting();

            // Ignore if this is not our supported target.
            if alt_setting.class() != CLASS ||
               alt_setting.subclass() != SUBCLASS
            {
                continue;
            }

            // Check protocol version.
            let protocol = alt_setting.protocol();
            if protocol != PROTOCOL {
                bail!("Wrong protocol version: {} supported, {} found",
                      PROTOCOL, protocol);
            }

            // Try to claim the interface.
            let interface = device
                .claim_interface(interface_number)
                .context("Failed to claim interface")?;

            // Select the required alternate, if not the default.
            if alt_setting_number != 0 {
                interface
                    .set_alt_setting(alt_setting_number)
                    .context("Failed to select alternate setting")?;
            }

            // Fetch the available speeds.
            let handle = CynthionHandle { interface };
            let speeds = handle
                .speeds()
                .context("Failed to fetch available speeds")?;

            // Now we have a usable device.
            return Ok((
                InterfaceSelection {
                    interface_number,
                    alt_setting_number,
                },
                speeds
            ))
        }
    }

    bail!("No supported analyzer interface found");
}

impl CynthionDevice {
    pub fn scan() -> Result<Vec<CynthionDevice>, Error> {
        Ok(nusb::list_devices()?
            .filter(|info| info.vendor_id() == VID)
            .filter(|info| info.product_id() == PID)
            .map(|device_info|
                match check_device(&device_info) {
                    Ok((iface, speeds)) => CynthionDevice {
                        device_info,
                        usability: Usable(iface, speeds)
                    },
                    Err(err) => CynthionDevice {
                        device_info,
                        usability: Unusable(format!("{}", err))
                    }
                }
            )
            .collect())
    }

    pub fn open(&self) -> Result<CynthionHandle, Error> {
        match &self.usability {
            Usable(iface, _) => {
                let device = self.device_info.open()?;
                let interface = device.claim_interface(iface.interface_number)?;
                if iface.alt_setting_number != 0 {
                    interface.set_alt_setting(iface.alt_setting_number)?;
                }
                Ok(CynthionHandle { interface })
            },
            Unusable(reason) => bail!("Device not usable: {}", reason),
        }
    }
}

impl CynthionHandle {

    pub fn speeds(&self) -> Result<Vec<Speed>, Error> {
        use Speed::*;
        let control = Control {
            control_type: ControlType::Vendor,
            recipient: Recipient::Interface,
            request: 2,
            value: 0,
            index: self.interface.interface_number() as u16,
        };
        let mut buf = [0; 64];
        let timeout = Duration::from_secs(1);
        let size = self.interface
            .control_in_blocking(control, &mut buf, timeout)
            .context("Failed retrieving supported speeds from device")?;
        if size != 1 {
            bail!("Expected 1-byte response to speed request, got {size}");
        }
        let mut speeds = Vec::new();
        for speed in [Auto, High, Full, Low] {
            if buf[0] & speed.mask() != 0 {
                speeds.push(speed);
            }
        }
        Ok(speeds)
    }

    pub fn start<F>(mut self, speed: Speed, result_handler: F)
        -> Result<(CynthionStream, CynthionStop), Error>
        where F: FnOnce(Result<(), Error>) + Send + 'static
    {
        // Channel to pass captured data to the decoder thread.
        let (tx, rx) = mpsc::channel();
        // Channel to stop the capture thread on request.
        let (stop_tx, mut stop_rx) = oneshot::channel();
        // Capture thread.
        let run_capture = move || {
            let mut state = State::new(true, speed);
            self.write_state(state)?;
            println!("Capture enabled, speed: {}", speed.description());
            let mut stopped = false;

            // Set up transfer queue.
            let mut data_transfer_queue = self.interface.bulk_in_queue(ENDPOINT);
            while data_transfer_queue.pending() < NUM_TRANSFERS {
                data_transfer_queue.submit(RequestBuffer::new(READ_LEN));
            }

            // Set up capture task.
            let capture_task = async move {
                loop {
                    select_biased!(
                        _ = stop_rx => {
                            // Capture stop requested. Cancel all transfers.
                            data_transfer_queue.cancel_all();
                            stopped = true;
                        }
                        completion = data_transfer_queue.next_complete().fuse() => {
                            match completion.status {
                                Ok(()) => {
                                    // Transfer successful.
                                    if !stopped {
                                        // Send data to decoder thread.
                                        tx.send(completion.data)
                                            .context("Failed sending capture data to channel")?;
                                        // Submit next transfer.
                                        data_transfer_queue.submit(RequestBuffer::new(READ_LEN));
                                    }
                                },
                                Err(TransferError::Cancelled) if stopped => {
                                    // Transfer cancelled during shutdown. Drop it.
                                    drop(completion);
                                    if data_transfer_queue.pending() == 0 {
                                        // All cancellations now handled.
                                        return Ok(());
                                    }
                                },
                                Err(usb_error) => {
                                    // Transfer failed.
                                    return Err(Error::from(usb_error));
                                }
                            }
                        }
                    );
                }
            };

            // Run capture task to completion.
            block_on(capture_task)?;

            // Stop capture.
            state.set_enable(false);
            self.write_state(state)?;
            println!("Capture disabled");
            Ok(())
        };
        let worker = spawn(move || result_handler(run_capture()));
        Ok((
            CynthionStream {
                receiver: rx,
                buffer: VecDeque::new(),
            },
            CynthionStop {
                stop_request: stop_tx,
                worker,
            }
        ))
    }

    fn write_state(&mut self, state: State) -> Result<(), Error> {
        let control = Control {
            control_type: ControlType::Vendor,
            recipient: Recipient::Interface,
            request: 1,
            value: u16::from(state.0),
            index: self.interface.interface_number() as u16,
        };
        let data = &[];
        let timeout = Duration::from_secs(1);
        self.interface
            .control_out_blocking(control, data, timeout)
            .context("Failed writing state to device")?;
        Ok(())
    }
}

impl Iterator for CynthionStream {
    type Item = Vec<u8>;

    fn next(&mut self) -> Option<Vec<u8>> {
        loop {
            // Do we have another packet already in the buffer?
            match self.next_buffered_packet() {
                // Yes; return the packet.
                Some(packet) => return Some(packet),
                // No; wait for more data from the capture thread.
                None => match self.receiver.recv().ok() {
                    // Received more data; add it to the buffer and retry.
                    Some(bytes) => self.buffer.extend(bytes.iter()),
                    // Capture has ended, there are no more packets.
                    None => return None
                }
            }
        }
    }
}

impl CynthionStream {
    fn next_buffered_packet(&mut self) -> Option<Vec<u8>> {
        // Do we have the length header for the next packet?
        let buffer_len = self.buffer.len();
        if buffer_len <= 2 {
            return None;
        }

        // Do we have all the data for the next packet?
        let packet_len = u16::from_be_bytes(
            [self.buffer[0], self.buffer[1]]) as usize;
        if buffer_len <= 2 + packet_len {
            return None;
        }

        // Remove the length header from the buffer.
        self.buffer.drain(0..2);

        // Remove the packet from the buffer and return it.
        Some(self.buffer.drain(0..packet_len).collect())
    }
}

impl CynthionStop {
    pub fn stop(self) -> Result<(), Error> {
        println!("Requesting capture stop");
        self.stop_request.send(())
            .or_else(|_| bail!("Failed sending stop request"))?;
        match self.worker.join() {
            Ok(()) => Ok(()),
            Err(panic) => {
                let msg = match (
                    panic.downcast_ref::<&str>(),
                    panic.downcast_ref::<String>())
                {
                    (Some(&s), _) => s,
                    (_,  Some(s)) => s,
                    (None,  None) => "<No panic message>"
                };
                bail!("Worker thread panic: {msg}");
            }
        }
    }
}
