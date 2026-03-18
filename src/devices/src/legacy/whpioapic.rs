// Copyright 2024 The libkrun Authors. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
//
// WHP interrupt controller for Windows.
// Follows the KvmIoapic pattern: a simple IrqChipT that signals
// interrupts via EventFd. WHP handles interrupt routing internally.

use std::io;

use crate::bus::BusDevice;
use crate::legacy::irqchip::IrqChipT;
use crate::Error as DeviceError;

use utils::eventfd::EventFd;

pub struct WhpIoapic {}

impl WhpIoapic {
    pub fn new() -> Self {
        Self {}
    }
}

impl IrqChipT for WhpIoapic {
    fn get_mmio_addr(&self) -> u64 {
        0
    }

    fn get_mmio_size(&self) -> u64 {
        0
    }

    fn set_irq(
        &self,
        _irq_line: Option<u32>,
        interrupt_evt: Option<&EventFd>,
    ) -> Result<(), DeviceError> {
        if let Some(interrupt_evt) = interrupt_evt {
            if let Err(e) = interrupt_evt.write(1) {
                error!("Failed to signal used queue: {e:?}");
                return Err(DeviceError::FailedSignalingUsedQueue(e));
            }
        } else {
            error!("EventFd not set up for irq line");
            return Err(DeviceError::FailedSignalingUsedQueue(io::Error::new(
                io::ErrorKind::NotFound,
                "EventFd not set up for irq line",
            )));
        }
        Ok(())
    }
}

impl BusDevice for WhpIoapic {
    fn read(&mut self, _vcpuid: u64, _offset: u64, _data: &mut [u8]) {
        // WHP handles IOAPIC emulation internally
    }

    fn write(&mut self, _vcpuid: u64, _offset: u64, _data: &[u8]) {
        // WHP handles IOAPIC emulation internally
    }
}
