// Copyright 2024 The libkrun Authors. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0

//! EventFd emulation using Windows Event objects.

use std::os::windows::io::{AsRawHandle, RawHandle};
use std::{io, result};

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{
    CreateEventW, ResetEvent, SetEvent, WaitForSingleObject, INFINITE,
};

pub const EFD_NONBLOCK: i32 = 1;
pub const EFD_SEMAPHORE: i32 = 2;

#[derive(Debug)]
pub struct EventFd {
    handle: HANDLE,
    nonblock: bool,
}

// SAFETY: HANDLE is just a pointer-sized value that Windows guarantees
// is safe to use from any thread.
unsafe impl Send for EventFd {}
unsafe impl Sync for EventFd {}

impl EventFd {
    pub fn new(flag: i32) -> result::Result<EventFd, io::Error> {
        let handle = unsafe { CreateEventW(None, true, false, None) }
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        Ok(EventFd {
            handle,
            nonblock: flag & EFD_NONBLOCK != 0,
        })
    }

    pub fn write(&self, _v: u64) -> result::Result<(), io::Error> {
        unsafe { SetEvent(self.handle) }
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))
    }

    pub fn read(&self) -> result::Result<u64, io::Error> {
        let timeout = if self.nonblock { 0 } else { INFINITE };
        let result = unsafe { WaitForSingleObject(self.handle, timeout) };
        if result == WAIT_OBJECT_0 {
            unsafe { ResetEvent(self.handle) }
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
            Ok(1)
        } else if self.nonblock {
            Err(io::Error::new(io::ErrorKind::WouldBlock, "would block"))
        } else {
            Err(io::Error::new(io::ErrorKind::Other, "wait failed"))
        }
    }

    pub fn try_clone(&self) -> result::Result<EventFd, io::Error> {
        use windows::Win32::Foundation::{DuplicateHandle, DUPLICATE_SAME_ACCESS};
        use windows::Win32::System::Threading::GetCurrentProcess;

        let mut new_handle = HANDLE::default();
        let current = unsafe { GetCurrentProcess() };
        unsafe {
            DuplicateHandle(
                current,
                self.handle,
                current,
                &mut new_handle,
                0,
                false,
                DUPLICATE_SAME_ACCESS,
            )
        }
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        Ok(EventFd {
            handle: new_handle,
            nonblock: self.nonblock,
        })
    }

    pub fn get_write_fd(&self) -> RawHandle {
        self.handle.0 as RawHandle
    }
}

impl AsRawHandle for EventFd {
    fn as_raw_handle(&self) -> RawHandle {
        self.handle.0 as RawHandle
    }
}

impl Drop for EventFd {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.handle) };
    }
}
