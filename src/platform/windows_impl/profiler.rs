use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::RwLock;
use std::thread::JoinHandle;
use std::time::SystemTime;

use once_cell::sync::Lazy;
use smallvec::SmallVec;

use crate::backtrace::{Trace, TraceImpl};
use crate::error::Result;
use crate::profiler::PROFILER;
use crate::MAX_DEPTH;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcessId, GetCurrentThreadId, OpenThread, THREAD_GET_CONTEXT,
    THREAD_QUERY_INFORMATION, THREAD_SUSPEND_RESUME,
};

static WORKER_THREAD: Lazy<RwLock<Option<JoinHandle<()>>>> = Lazy::new(|| RwLock::new(None));
static WORKER_TERMINATED: AtomicBool = AtomicBool::new(false);

pub fn register() -> Result<()> {
    // Main thread is refering to the calling thread.
    // Which in most cases should be main anyways.
    let main_thread_id = unsafe { GetCurrentThreadId() };

    WORKER_TERMINATED.store(false, Ordering::Relaxed);

    if let Ok(mut thread) = WORKER_THREAD.try_write() {
        if thread.is_none() {
            let handle = std::thread::spawn(move || perf_worker_thread(main_thread_id));
            *thread = Some(handle);
        }
    }
    Ok(())
}
pub fn unregister() -> Result<()> {
    WORKER_TERMINATED.store(true, Ordering::Relaxed);

    if let Ok(mut thread) = WORKER_THREAD.try_write() {
        if let Some(thread) = thread.take() {
            // TODO: silent fail is a bad idea
            let _ = thread.join();
        }
    }

    Ok(())
}

struct ThreadIds {
    first: bool,
    snapshot_handle: HANDLE,
}
impl ThreadIds {
    fn new() -> Self {
        Self {
            first: true,
            snapshot_handle: unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) },
        }
    }
}
impl Drop for ThreadIds {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.snapshot_handle);
        }
    }
}
impl Iterator for ThreadIds {
    type Item = u32;

    fn next(&mut self) -> Option<Self::Item> {
        let current_process = unsafe { GetCurrentProcessId() as u32 };

        loop {
            unsafe {
                let mut thread_entry: THREADENTRY32 = std::mem::zeroed();
                thread_entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;

                let status = if self.first {
                    Thread32First(self.snapshot_handle, &mut thread_entry)
                } else {
                    Thread32Next(self.snapshot_handle, &mut thread_entry)
                };
                self.first = false;

                if status == 0 {
                    return None;
                } else if thread_entry.th32OwnerProcessID == current_process {
                    return Some(thread_entry.th32ThreadID);
                }
            }
        }
    }
}

fn perf_worker_thread(main_thread_id: u32) {
    let worker_id = unsafe { GetCurrentThreadId() };

    loop {
        if WORKER_TERMINATED.load(Ordering::Relaxed) {
            break;
        }

        if let Some(mut guard) = PROFILER.try_write() {
            if let Ok(profiler) = guard.as_mut() {
                for id in ThreadIds::new() {
                    if id == worker_id {
                        continue;
                    }

                    let mut bt: SmallVec<[<TraceImpl as Trace>::Frame; MAX_DEPTH]> =
                        SmallVec::with_capacity(MAX_DEPTH);
                    let mut index = 0;

                    unsafe {
                        const THREAD_ACCESS: u32 =
                            THREAD_GET_CONTEXT | THREAD_QUERY_INFORMATION | THREAD_SUSPEND_RESUME;
                        let handle = OpenThread(THREAD_ACCESS, 0, id) as usize;

                        backtrace::trace_thread_unsynchronized(handle as _, |frame| {
                            if index < MAX_DEPTH {
                                bt.push(frame.clone());
                                index += 1;
                                true
                            } else {
                                false
                            }
                        });
                    }

                    let name = if id == main_thread_id {
                        String::from("main")
                    } else {
                        format!("{:x}", id)
                    };

                    let timestamp = SystemTime::now();
                    profiler.sample(bt, name.as_bytes(), id as u64, timestamp);
                }
            }
        }
    }
}
