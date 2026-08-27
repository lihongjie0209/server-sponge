use log::{debug, info, warn};
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;

/// Pressure level detected from PSI
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PressureLevel {
    /// No memory pressure
    None,
    /// Some tasks are stalling (moderate pressure)
    Some,
    /// All tasks are stalling (severe pressure)
    Full,
}

/// PSI monitor that uses epoll to listen for kernel memory pressure events.
pub struct PsiMonitor {
    _some_fd: Option<File>,
    _full_fd: Option<File>,
    epoll_fd: Option<i32>,
}

// epoll constants (from Linux kernel headers)
const EPOLL_CTL_ADD: i32 = 1;
const EPOLLPRI: u32 = 0x002;

#[repr(C)]
#[derive(Clone, Copy)]
struct EpollEvent {
    events: u32,
    data: u64,
}

impl PsiMonitor {
    /// Try to initialize PSI monitoring. Returns a degraded instance if PSI is unavailable.
    pub fn new(some_threshold_us: u64, window_us: u64) -> Self {
        match Self::try_init(some_threshold_us, window_us) {
            Ok(monitor) => {
                info!("PSI monitoring initialized successfully");
                monitor
            }
            Err(e) => {
                warn!("PSI unavailable ({}), falling back to polling mode", e);
                Self {
                    _some_fd: None,
                    _full_fd: None,
                    epoll_fd: None,
                }
            }
        }
    }

    fn try_init(some_threshold_us: u64, window_us: u64) -> io::Result<Self> {
        // Open and configure "some" trigger
        let some_fd = Self::setup_trigger("some", some_threshold_us, window_us)?;

        // Open and configure "full" trigger with a lower threshold
        let full_threshold = some_threshold_us / 2; // More sensitive for full stall
        let full_fd = Self::setup_trigger("full", full_threshold, window_us)?;

        // Create epoll instance
        let epoll_fd = unsafe { libc::epoll_create1(0) };
        if epoll_fd < 0 {
            return Err(io::Error::last_os_error());
        }

        // Register both fds with epoll
        Self::epoll_add(epoll_fd, some_fd.as_raw_fd(), 1)?;
        Self::epoll_add(epoll_fd, full_fd.as_raw_fd(), 2)?;

        Ok(Self {
            _some_fd: Some(some_fd),
            _full_fd: Some(full_fd),
            epoll_fd: Some(epoll_fd),
        })
    }

    fn setup_trigger(level: &str, threshold_us: u64, window_us: u64) -> io::Result<File> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/proc/pressure/memory")?;

        let trigger = format!("{} {} {}\0", level, threshold_us, window_us);
        file.write_all(trigger.as_bytes())?;
        Ok(file)
    }

    fn epoll_add(epoll_fd: i32, fd: i32, id: u64) -> io::Result<()> {
        let mut event = EpollEvent {
            events: EPOLLPRI,
            data: id,
        };
        let ret = unsafe {
            libc::epoll_ctl(
                epoll_fd,
                EPOLL_CTL_ADD,
                fd,
                &mut event as *mut EpollEvent as *mut libc::epoll_event,
            )
        };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Check if PSI monitoring is active
    pub fn is_available(&self) -> bool {
        self.epoll_fd.is_some()
    }

    /// Poll for pressure events with a timeout (milliseconds).
    /// Returns the highest pressure level detected.
    pub fn poll(&self, timeout_ms: i32) -> PressureLevel {
        let epoll_fd = match self.epoll_fd {
            Some(fd) => fd,
            None => return PressureLevel::None,
        };

        let mut events = [EpollEvent { events: 0, data: 0 }; 4];
        let n = unsafe {
            libc::epoll_wait(
                epoll_fd,
                events.as_mut_ptr() as *mut libc::epoll_event,
                events.len() as i32,
                timeout_ms,
            )
        };

        if n <= 0 {
            return PressureLevel::None;
        }

        let mut max_level = PressureLevel::None;
        for event in events.iter().take(n as usize) {
            match event.data {
                1 => {
                    debug!("PSI: 'some' pressure detected");
                    if max_level == PressureLevel::None {
                        max_level = PressureLevel::Some;
                    }
                }
                2 => {
                    debug!("PSI: 'full' pressure detected");
                    max_level = PressureLevel::Full;
                }
                _ => {}
            }
        }

        max_level
    }
}

impl Drop for PsiMonitor {
    fn drop(&mut self) {
        if let Some(fd) = self.epoll_fd {
            unsafe {
                libc::close(fd);
            }
        }
        // Files are closed automatically when dropped
    }
}
