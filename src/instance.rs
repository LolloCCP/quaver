//! Lets only one Quaver run: two would fight over Soloist, and both be called "Quaver" in Spotify.

use std::{ffi::c_void, io, ptr::null_mut};

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateEventW(security: *mut c_void, manual_reset: i32, signaled: i32, name: *const u16) -> *mut c_void;
    fn SetEvent(event: *mut c_void) -> i32;
    fn WaitForSingleObject(event: *mut c_void, ms: u32) -> u32;
}

/// A named event, which Windows forgets together with the last program that has it open.
pub struct Instance(usize);

/// Nothing if another program already has this name; that one is told.
pub fn claim(name: &str) -> Option<Instance> {
    const ERROR_ALREADY_EXISTS: i32 = 183;
    let wide: Vec<u16> = name.encode_utf16().chain([0]).collect();
    let event = unsafe { CreateEventW(null_mut(), 0, 0, wide.as_ptr()) };
    let error = io::Error::last_os_error();
    if event.is_null() {
        log::warn!("instance: {error}");
    } else if error.raw_os_error() == Some(ERROR_ALREADY_EXISTS) {
        unsafe { SetEvent(event) };
        return None;
    }
    Some(Instance(event as usize))
}

impl Instance {
    /// Calls this each time another one was started.
    pub fn on_another(self, told: impl Fn() + Send + 'static) {
        std::thread::spawn(move || {
            while unsafe { WaitForSingleObject(self.0 as *mut c_void, u32::MAX) } == 0 {
                told();
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::mpsc, time::Duration};

    #[test]
    fn the_first_is_told_about_the_second() {
        let name = format!("Local\\quaver-test-{}", std::process::id());
        let first = super::claim(&name).expect("nobody has it yet");
        let (tx, rx) = mpsc::channel();
        first.on_another(move || tx.send(()).unwrap());
        assert!(super::claim(&name).is_none());
        rx.recv_timeout(Duration::from_secs(5)).expect("told");
        assert!(rx.recv_timeout(Duration::from_millis(50)).is_err(), "once");
    }
}
