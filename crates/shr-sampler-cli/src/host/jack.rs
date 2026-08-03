use super::callback::{CallbackAdapter, HostFault, HostSignals, ScheduledEvent};
use super::queue::Consumer;
use shr_sampler_core::{Event, PreparedInstrument};
use std::ffi::CString;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use libc::{c_char, c_int, c_uint, c_ulong, c_void};

pub const JACK_OUTPUT_LEFT: &str = "out_l";
pub const JACK_OUTPUT_RIGHT: &str = "out_r";

const JACK_DEFAULT_AUDIO_TYPE: &[u8] = b"32 bit float mono audio\0";
const JACK_PORT_IS_OUTPUT: c_ulong = 2;
const JACK_NO_START_SERVER: c_uint = 1;
const JACK_USE_EXACT_NAME: c_uint = 2;
const JACK_OPEN_OPTIONS: c_uint = JACK_NO_START_SERVER | JACK_USE_EXACT_NAME;
const PANIC_ACK_TIMEOUT: Duration = Duration::from_millis(100);

#[repr(C)]
struct OpaqueClient {
    _private: [u8; 0],
}

#[repr(C)]
struct Port {
    _private: [u8; 0],
}

type ClientOpen =
    unsafe extern "C" fn(*const c_char, c_uint, *mut c_uint, ...) -> *mut OpaqueClient;
type ClientClose = unsafe extern "C" fn(*mut OpaqueClient) -> c_int;
type PortRegister = unsafe extern "C" fn(
    *mut OpaqueClient,
    *const c_char,
    *const c_char,
    c_ulong,
    c_ulong,
) -> *mut Port;
type SetProcess = unsafe extern "C" fn(*mut OpaqueClient, ProcessCallback, *mut c_void) -> c_int;
type OnShutdown = unsafe extern "C" fn(*mut OpaqueClient, ShutdownCallback, *mut c_void);
type Activate = unsafe extern "C" fn(*mut OpaqueClient) -> c_int;
type Deactivate = unsafe extern "C" fn(*mut OpaqueClient) -> c_int;
type SampleRate = unsafe extern "C" fn(*const OpaqueClient) -> c_uint;
type PortGetBuffer = unsafe extern "C" fn(*mut Port, c_uint) -> *mut c_void;
type FramesSinceCycleStart = unsafe extern "C" fn(*const OpaqueClient) -> c_uint;
type ProcessCallback = unsafe extern "C" fn(c_uint, *mut c_void) -> c_int;
type ShutdownCallback = unsafe extern "C" fn(*mut c_void);

#[derive(Clone)]
pub(crate) struct Timing {
    client: *mut OpaqueClient,
    frames_since_cycle_start: FramesSinceCycleStart,
    cycle: Arc<AtomicU64>,
}

// JACK explicitly permits the frame-time query from a non-process thread.
unsafe impl Send for Timing {}

impl Timing {
    pub fn schedule(&self, event: Event) -> ScheduledEvent {
        let cycle = self.cycle.load(Ordering::Acquire).saturating_add(1);
        // SAFETY: the retained JackHost keeps the client and symbol alive for
        // longer than the MIDI thread which owns this Timing value.
        let sample_offset = unsafe { (self.frames_since_cycle_start)(self.client) as usize };
        ScheduledEvent {
            cycle,
            sample_offset,
            event,
        }
    }
}

#[derive(Clone, Copy)]
struct Api {
    handle: *mut c_void,
    close: ClientClose,
    register: PortRegister,
    set_process: SetProcess,
    on_shutdown: OnShutdown,
    activate: Activate,
    deactivate: Deactivate,
    sample_rate: SampleRate,
    get_buffer: PortGetBuffer,
    frames_since_cycle_start: FramesSinceCycleStart,
}

struct CallbackState<'a> {
    adapter: CallbackAdapter<'a>,
    left: *mut Port,
    right: *mut Port,
    get_buffer: PortGetBuffer,
    signals: HostSignals,
}

pub(crate) struct JackHost<'a> {
    client: *mut OpaqueClient,
    api: Api,
    state: Box<CallbackState<'a>>,
    active: bool,
}

impl<'a> JackHost<'a> {
    pub fn open(
        client_name: &str,
        instrument: &'a PreparedInstrument,
        queue: Consumer<ScheduledEvent>,
        signals: HostSignals,
    ) -> Result<(Self, Timing), String> {
        let name = CString::new(client_name)
            .map_err(|_| "JACK client name contains a NUL byte".to_string())?;
        // SAFETY: each resolved symbol remains backed by the retained handle.
        unsafe {
            let handle = libc::dlopen(c"libjack.so.0".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
            if handle.is_null() {
                return Err("JACK library libjack.so.0 is unavailable".into());
            }
            let loaded = (|| -> Result<(ClientOpen, Api), String> {
                Ok((
                    symbol(handle, b"jack_client_open\0")?,
                    Api {
                        handle,
                        close: symbol(handle, b"jack_client_close\0")?,
                        register: symbol(handle, b"jack_port_register\0")?,
                        set_process: symbol(handle, b"jack_set_process_callback\0")?,
                        on_shutdown: symbol(handle, b"jack_on_shutdown\0")?,
                        activate: symbol(handle, b"jack_activate\0")?,
                        deactivate: symbol(handle, b"jack_deactivate\0")?,
                        sample_rate: symbol(handle, b"jack_get_sample_rate\0")?,
                        get_buffer: symbol(handle, b"jack_port_get_buffer\0")?,
                        frames_since_cycle_start: symbol(
                            handle,
                            b"jack_frames_since_cycle_start\0",
                        )?,
                    },
                ))
            })();
            let (open, api) = match loaded {
                Ok(loaded) => loaded,
                Err(error) => {
                    libc::dlclose(handle);
                    return Err(error);
                }
            };
            let mut status = 0;
            let client = open(name.as_ptr(), JACK_OPEN_OPTIONS, &mut status);
            if client.is_null() {
                libc::dlclose(handle);
                return Err(format!(
                    "JACK server/client setup failed without starting a server (status {status})"
                ));
            }
            let result = (|| -> Result<(Self, Timing), String> {
                let left = register_output(&api, client, JACK_OUTPUT_LEFT)?;
                let right = register_output(&api, client, JACK_OUTPUT_RIGHT)?;
                let sample_rate = (api.sample_rate)(client);
                let cycle = Arc::new(AtomicU64::new(0));
                let timing = Timing {
                    client,
                    frames_since_cycle_start: api.frames_since_cycle_start,
                    cycle: cycle.clone(),
                };
                let adapter = CallbackAdapter::new(
                    instrument,
                    sample_rate as f32,
                    queue,
                    cycle,
                    signals.clone(),
                )
                .map_err(|error| format!("prepare sampler engine for JACK sample rate: {error}"))?;
                let mut state = Box::new(CallbackState {
                    adapter,
                    left,
                    right,
                    get_buffer: api.get_buffer,
                    signals,
                });
                if (api.set_process)(
                    client,
                    process_callback,
                    (&mut *state as *mut CallbackState<'_>).cast(),
                ) != 0
                {
                    return Err("register JACK process callback".into());
                }
                (api.on_shutdown)(
                    client,
                    shutdown_callback,
                    (&mut *state as *mut CallbackState<'_>).cast(),
                );
                Ok((
                    Self {
                        client,
                        api,
                        state,
                        active: false,
                    },
                    timing,
                ))
            })();
            match result {
                Ok(value) => Ok(value),
                Err(error) => {
                    (api.close)(client);
                    libc::dlclose(api.handle);
                    Err(error)
                }
            }
        }
    }

    pub fn activate(&mut self) -> Result<(), String> {
        if unsafe { (self.api.activate)(self.client) } != 0 {
            return Err("activate JACK client".into());
        }
        self.active = true;
        Ok(())
    }

    pub fn shutdown_owned(&mut self) {
        if self.active {
            let request = self.state.signals.request_all_notes_off();
            let deadline = Instant::now() + PANIC_ACK_TIMEOUT;
            while self.state.signals.panic_acknowledged() < request
                && self.state.signals.fault().is_none()
                && Instant::now() < deadline
            {
                std::thread::sleep(Duration::from_millis(1));
            }
            // SAFETY: deactivation synchronizes with any active process call.
            unsafe { (self.api.deactivate)(self.client) };
            self.active = false;
        }
        // The callback is inactive here, so this final owned-state cleanup is
        // race-free even when JACK shutdown prevented the acknowledgement.
        self.state.adapter.force_all_notes_off();
    }
}

impl Drop for JackHost<'_> {
    fn drop(&mut self) {
        self.shutdown_owned();
        unsafe {
            (self.api.close)(self.client);
            libc::dlclose(self.api.handle);
        }
    }
}

unsafe fn symbol<T: Copy>(handle: *mut c_void, name: &[u8]) -> Result<T, String> {
    let pointer = unsafe { libc::dlsym(handle, name.as_ptr().cast()) };
    if pointer.is_null() {
        return Err(format!(
            "JACK symbol {} is unavailable",
            String::from_utf8_lossy(&name[..name.len().saturating_sub(1)])
        ));
    }
    Ok(unsafe { std::mem::transmute_copy(&pointer) })
}

unsafe fn register_output(
    api: &Api,
    client: *mut OpaqueClient,
    name: &str,
) -> Result<*mut Port, String> {
    let name = CString::new(name).map_err(|_| "static JACK port name contains NUL".to_string())?;
    let port = unsafe {
        (api.register)(
            client,
            name.as_ptr(),
            JACK_DEFAULT_AUDIO_TYPE.as_ptr().cast(),
            JACK_PORT_IS_OUTPUT,
            0,
        )
    };
    if port.is_null() {
        return Err(format!("register JACK output {}", name.to_string_lossy()));
    }
    Ok(port)
}

unsafe extern "C" fn shutdown_callback(argument: *mut c_void) {
    if let Some(state) = unsafe { argument.cast::<CallbackState<'_>>().as_ref() } {
        state.signals.publish_fault(HostFault::JackShutdown);
    }
}

unsafe extern "C" fn process_callback(frames: c_uint, argument: *mut c_void) -> c_int {
    let Some(state) = (unsafe { argument.cast::<CallbackState<'_>>().as_mut() }) else {
        return 0;
    };
    let frames = frames as usize;
    if frames == 0 {
        return 0;
    }
    let left = unsafe { (state.get_buffer)(state.left, frames as c_uint).cast::<f32>() };
    let right = unsafe { (state.get_buffer)(state.right, frames as c_uint).cast::<f32>() };
    if left.is_null() || right.is_null() {
        if !left.is_null() {
            unsafe { std::slice::from_raw_parts_mut(left, frames) }.fill(0.0);
        }
        if !right.is_null() {
            unsafe { std::slice::from_raw_parts_mut(right, frames) }.fill(0.0);
        }
        state.adapter.force_all_notes_off();
        state.signals.publish_fault(HostFault::CallbackBuffers);
        return 0;
    }
    let left = unsafe { std::slice::from_raw_parts_mut(left, frames) };
    let right = unsafe { std::slice::from_raw_parts_mut(right, frames) };
    state.adapter.process(left, right);
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_jack_names_are_exact() {
        assert_eq!(JACK_OUTPUT_LEFT, "out_l");
        assert_eq!(JACK_OUTPUT_RIGHT, "out_r");
    }
}
