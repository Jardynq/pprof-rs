// Copyright 2019 TiKV Project Authors. Licensed under Apache-2.0.

use std::convert::TryInto;
use std::os::raw::c_int;

use backtrace::Frame;
//use nix::sys::signal;
use once_cell::sync::Lazy;
use parking_lot::RwLock;
use smallvec::SmallVec;

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
use findshlibs::{Segment, SharedLibrary, TargetSharedLibrary};

use crate::collector::Collector;
use crate::error::{Error, Result};
use crate::frames::UnresolvedFrames;
use crate::report::ReportBuilder;
use crate::timer::Timer;
use crate::{MAX_DEPTH, MAX_THREAD_NAME};

pub(crate) static PROFILER: Lazy<RwLock<Result<Profiler>>> =
    Lazy::new(|| RwLock::new(Profiler::new()));

pub struct Profiler {
    pub(crate) data: Collector<UnresolvedFrames>,
    sample_counter: i32,

    running: bool,

    #[cfg(all(any(target_arch = "x86_64", target_arch = "aarch64")))]
    blocklist_segments: Vec<(usize, usize)>,
}

#[derive(Clone)]
pub struct ProfilerGuardBuilder {
    frequency: c_int,
    #[cfg(all(any(target_arch = "x86_64", target_arch = "aarch64")))]
    blocklist_segments: Vec<(usize, usize)>,
}

impl Default for ProfilerGuardBuilder {
    fn default() -> ProfilerGuardBuilder {
        ProfilerGuardBuilder {
            frequency: 99,

            #[cfg(all(any(target_arch = "x86_64", target_arch = "aarch64")))]
            blocklist_segments: Vec::new(),
        }
    }
}

impl ProfilerGuardBuilder {
    pub fn frequency(self, frequency: c_int) -> Self {
        Self { frequency, ..self }
    }
    #[cfg(all(any(target_arch = "x86_64", target_arch = "aarch64")))]
    pub fn blocklist<T: AsRef<str>>(self, blocklist: &[T]) -> Self {
        let blocklist_segments = {
            let mut segments = Vec::new();
            TargetSharedLibrary::each(|shlib| {
                let in_blocklist = match shlib.name().to_str() {
                    Some(name) => {
                        let mut in_blocklist = false;
                        for blocked_name in blocklist.iter() {
                            if name.contains(blocked_name.as_ref()) {
                                in_blocklist = true;
                            }
                        }

                        in_blocklist
                    }

                    None => false,
                };
                if in_blocklist {
                    for seg in shlib.segments() {
                        let avam = seg.actual_virtual_memory_address(shlib);
                        let start = avam.0;
                        let end = start + seg.len();
                        segments.push((start, end));
                    }
                }
            });
            segments
        };

        Self {
            blocklist_segments,
            ..self
        }
    }
    pub fn build(self) -> Result<ProfilerGuard<'static>> {
        trigger_lazy();

        match PROFILER.write().as_mut() {
            Err(err) => {
                log::error!("Error in creating profiler: {}", err);
                Err(Error::CreatingError)
            }
            Ok(profiler) => {
                #[cfg(all(any(target_arch = "x86_64", target_arch = "aarch64")))]
                {
                    profiler.blocklist_segments = self.blocklist_segments;
                }

                match profiler.start() {
                    Ok(()) => Ok(ProfilerGuard::<'static> {
                        profiler: &PROFILER,
                        timer: Some(Timer::new(self.frequency)),
                    }),
                    Err(err) => Err(err),
                }
            }
        }
    }
}

/// RAII structure used to stop profiling when dropped. It is the only interface to access profiler.
pub struct ProfilerGuard<'a> {
    profiler: &'a RwLock<Result<Profiler>>,
    timer: Option<Timer>,
}

fn trigger_lazy() {
    let _ = backtrace::Backtrace::new();
    let _lock = PROFILER.read();
}

impl ProfilerGuard<'_> {
    /// Start profiling with given sample frequency.
    pub fn new(frequency: c_int) -> Result<ProfilerGuard<'static>> {
        ProfilerGuardBuilder::default().frequency(frequency).build()
    }

    /// Generate a report
    pub fn report(&self) -> ReportBuilder {
        ReportBuilder::new(
            self.profiler,
            self.timer.as_ref().map(Timer::timing).unwrap_or_default(),
        )
    }
}

impl<'a> Drop for ProfilerGuard<'a> {
    fn drop(&mut self) {
        drop(self.timer.take());

        match self.profiler.write().as_mut() {
            Err(_) => {}
            Ok(profiler) => match profiler.stop() {
                Ok(()) => {}
                Err(err) => log::error!("error while stopping profiler {}", err),
            },
        }
    }
}


fn perf_worker_thread() {
    let worker_id = unsafe { winapi::um::processthreadsapi::GetCurrentThreadId() };
    loop {
        let proc = winproc::Process::current();
        let threads = proc.thread_ids().unwrap();

        if let Some(mut guard) = PROFILER.try_write() {
            if let Ok(profiler) = guard.as_mut() {
                for id in threads {
                    if id == worker_id {
                        continue;
                    }

                    let mut bt: SmallVec<[Frame; MAX_DEPTH]> = SmallVec::with_capacity(MAX_DEPTH);
                    let mut index = 0;

                    unsafe {
                        const ALL_ACCESS: u32 = 2_097_151u32;
                        let handle = winapi::um::processthreadsapi::OpenThread(ALL_ACCESS, 0, id) as usize;
                        
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
                    
                    profiler.sample(bt, format!("{:x}", id).as_bytes(), id as u64);
                }
            }
        }
    }
}

impl Profiler {
    fn new() -> Result<Self> {
        Ok(Profiler {
            data: Collector::new()?,
            sample_counter: 0,
            running: false,

            #[cfg(all(any(target_arch = "x86_64", target_arch = "aarch64")))]
            blocklist_segments: Vec::new(),
        })
    }

    #[cfg(all(any(target_arch = "x86_64", target_arch = "aarch64")))]
    fn is_blocklisted(&self, addr: usize) -> bool {
        for libs in &self.blocklist_segments {
            if addr > libs.0 && addr < libs.1 {
                return true;
            }
        }
        false
    }
}

impl Profiler {
    pub fn start(&mut self) -> Result<()> {
        log::info!("starting cpu profiler");
        if self.running {
            Err(Error::Running)
        } else {
            self.register_signal_handler()?;
            self.running = true;

            Ok(())
        }
    }

    fn init(&mut self) -> Result<()> {
        self.sample_counter = 0;
        self.data = Collector::new()?;
        self.running = false;

        Ok(())
    }

    pub fn stop(&mut self) -> Result<()> {
        log::info!("stopping cpu profiler");
        if self.running {
            self.unregister_signal_handler()?;
            self.init()?;

            Ok(())
        } else {
            Err(Error::NotRunning)
        }
    }

    fn register_signal_handler(&self) -> Result<()> {
        std::thread::spawn(perf_worker_thread);
        Ok(())
    }

    fn unregister_signal_handler(&self) -> Result<()> {
        Ok(())
    }

    // This function has to be AS-safe
    pub fn sample(
        &mut self,
        backtrace: SmallVec<[Frame; MAX_DEPTH]>,
        thread_name: &[u8],
        thread_id: u64,
    ) {
        let frames = UnresolvedFrames::new(backtrace, thread_name, thread_id);
        self.sample_counter += 1;

        if let Ok(()) = self.data.add(frames, 1) {}
    }
}

#[cfg(test)]
#[cfg(target_os = "linux")]
mod tests {
    use super::*;

    use std::cell::RefCell;
    use std::ffi::c_void;

    use std::ptr::null_mut;

    extern "C" {
        static mut __malloc_hook: Option<extern "C" fn(size: usize) -> *mut c_void>;

        fn malloc(size: usize) -> *mut c_void;
    }

    thread_local! {
        static FLAG: RefCell<bool> = RefCell::new(false);
    }

    extern "C" fn malloc_hook(size: usize) -> *mut c_void {
        unsafe {
            __malloc_hook = None;
        }

        FLAG.with(|flag| {
            flag.replace(true);
        });
        let p = unsafe { malloc(size) };

        unsafe {
            __malloc_hook = Some(malloc_hook);
        }

        p
    }

    #[inline(never)]
    fn is_prime_number(v: usize, prime_numbers: &[usize]) -> bool {
        if v < 10000 {
            let r = prime_numbers.binary_search(&v);
            return r.is_ok();
        }

        for n in prime_numbers {
            if v % n == 0 {
                return false;
            }
        }

        true
    }

    #[inline(never)]
    fn prepare_prime_numbers() -> Vec<usize> {
        // bootstrap: Generate a prime table of 0..10000
        let mut prime_number_table: [bool; 10000] = [true; 10000];
        prime_number_table[0] = false;
        prime_number_table[1] = false;
        for i in 2..10000 {
            if prime_number_table[i] {
                let mut v = i * 2;
                while v < 10000 {
                    prime_number_table[v] = false;
                    v += i;
                }
            }
        }
        let mut prime_numbers = vec![];
        for (i, item) in prime_number_table.iter().enumerate().skip(2) {
            if *item {
                prime_numbers.push(i);
            }
        }
        prime_numbers
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn malloc_free() {
        trigger_lazy();

        let prime_numbers = prepare_prime_numbers();

        let mut _v = 0;

        unsafe {
            __malloc_hook = Some(malloc_hook);
        }
        for i in 2..50000 {
            if is_prime_number(i, &prime_numbers) {
                _v += 1;
                perf_signal_handler(27, null_mut(), null_mut());
            }
        }
        unsafe {
            __malloc_hook = None;
        }

        FLAG.with(|flag| {
            assert!(!*flag.borrow());
        });
    }
}
