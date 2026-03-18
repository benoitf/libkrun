// Copyright 2024 The libkrun Authors. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0

//! Epoll-like event notification emulation using WaitForMultipleObjects on Windows.

use std::collections::HashMap;
use std::io;
use std::os::windows::io::RawHandle;
use std::sync::Mutex;
use std::time::Duration;

use bitflags::bitflags;
use log::debug;
use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::System::Threading::WaitForMultipleObjects;

#[repr(i32)]
pub enum ControlOperation {
    Add,
    Modify,
    Delete,
}

bitflags! {
    pub struct EventSet: u32 {
        const IN = 0b00000001;
        const OUT = 0b00000010;
        const HANG_UP = 0b00000100;
        const READ_HANG_UP = 0b00001000;
        const EDGE_TRIGGERED = 0b00010000;
    }
}

#[derive(Clone, Copy, Default, Debug)]
pub struct EpollEvent {
    pub events: u32,
    u64: u64,
}

impl EpollEvent {
    pub fn new(events: EventSet, data: u64) -> Self {
        debug!("EpollEvent new: {data}");
        EpollEvent {
            events: events.bits(),
            u64: data,
        }
    }

    pub fn events(&self) -> u32 {
        self.events
    }

    pub fn event_set(&self) -> EventSet {
        EventSet::from_bits(self.events()).unwrap()
    }

    pub fn data(&self) -> u64 {
        debug!("EpollEvent data: {}", self.u64);
        self.u64
    }

    pub fn fd(&self) -> RawHandle {
        self.u64 as RawHandle
    }
}

#[derive(Clone, Debug)]
struct Registration {
    handle: HANDLE,
    event: EpollEvent,
}

// SAFETY: HANDLE is a pointer-sized value safe to send across threads.
unsafe impl Send for Registration {}
unsafe impl Sync for Registration {}

/// Epoll emulation using WaitForMultipleObjects.
///
/// Uses interior mutability (Mutex) so that `ctl` can take `&self`,
/// matching the Unix/macOS epoll API surface.
#[derive(Debug)]
pub struct Epoll {
    registrations: Mutex<HashMap<usize, Registration>>,
}

impl Clone for Epoll {
    fn clone(&self) -> Self {
        Epoll {
            registrations: Mutex::new(self.registrations.lock().unwrap().clone()),
        }
    }
}

impl Epoll {
    pub fn new() -> io::Result<Self> {
        Ok(Epoll {
            registrations: Mutex::new(HashMap::new()),
        })
    }

    pub fn ctl(
        &self,
        operation: ControlOperation,
        fd: RawHandle,
        event: &EpollEvent,
    ) -> io::Result<()> {
        let key = fd as usize;
        let mut regs = self.registrations.lock().unwrap();

        match operation {
            ControlOperation::Add => {
                debug!("epoll add handle: {:?}", fd);
                regs.insert(
                    key,
                    Registration {
                        handle: HANDLE(fd as *mut _),
                        event: *event,
                    },
                );
            }
            ControlOperation::Modify => {
                debug!("epoll modify handle: {:?}", fd);
                if let Some(reg) = regs.get_mut(&key) {
                    reg.event = *event;
                }
            }
            ControlOperation::Delete => {
                debug!("epoll delete handle: {:?}", fd);
                regs.remove(&key);
            }
        }
        Ok(())
    }

    pub fn wait(
        &self,
        max_events: usize,
        timeout: i32,
        events: &mut [EpollEvent],
    ) -> io::Result<usize> {
        let regs = self.registrations.lock().unwrap();

        if regs.is_empty() {
            drop(regs);
            if timeout > 0 {
                std::thread::sleep(Duration::from_millis(timeout as u64));
            }
            return Ok(0);
        }

        let handles: Vec<HANDLE> = regs.values().map(|r| r.handle).collect();
        let reg_events: Vec<EpollEvent> = regs.values().map(|r| r.event).collect();
        drop(regs);

        let timeout_ms = if timeout < 0 { 3000u32 } else { timeout as u32 };

        let result = unsafe { WaitForMultipleObjects(&handles, false, timeout_ms) };

        if result == WAIT_TIMEOUT {
            return Ok(0);
        }

        let index = (result.0 - WAIT_OBJECT_0.0) as usize;
        if index < handles.len() {
            let count = std::cmp::min(max_events, 1);
            if count > 0 && !events.is_empty() {
                events[0] = reg_events[index];
                events[0].events = EventSet::IN.bits();
            }
            Ok(count)
        } else {
            Err(io::Error::new(
                io::ErrorKind::Other,
                "WaitForMultipleObjects failed",
            ))
        }
    }

    pub fn as_raw_handle(&self) -> RawHandle {
        std::ptr::null_mut()
    }
}
