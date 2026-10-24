//! A PipeWire main loop owned by one worker thread for its whole life: a
//! copy of isocast-platform-linux's `pw_loop` (the app can't depend on the
//! platform crate), for the remote sound's output (`super::sound`).
//!
//! pipewire-rs's `Loop::iterate` enters and leaves the loop on every call.
//! While a loop is not entered it has no owning thread, and PipeWire 1.0.5's
//! `loop_invoke` then runs a cross-thread invoke inline on the caller
//! (`spa/plugins/support/loop.c`: `if (impl->thread == 0 || pthread_equal(...))
//! return loop_invoke_inthread(...)`). A non-RT stream posts its `process`
//! callback to the main loop from the data thread (`stream.c` `call_process`:
//! `pw_loop_invoke(impl->main_loop, do_call_process, ...)`), so between our
//! iterations that callback ran on PipeWire's data thread, concurrently with
//! the worker and its `Rc`/`RefCell` state.
//!
//! [`WorkerLoop`] enters once on the worker thread and leaves when dropped,
//! like `pw_main_loop_run`. Invokes from other threads are then always queued
//! and dispatched by [`WorkerLoop::iterate`] on the worker thread.

use std::marker::PhantomData;
use std::ops::Deref;
use std::time::Duration;

use anyhow::{Result, ensure};
use pipewire as pw;

pub(super) struct WorkerLoop {
    mainloop: pw::main_loop::MainLoopRc,
    // Entered on this thread: never moved to or dropped on another one.
    _thread: PhantomData<*const ()>,
}

impl WorkerLoop {
    pub(super) fn new() -> Result<Self> {
        let mainloop = pw::main_loop::MainLoopRc::new(None)?;
        // SAFETY: paired with `leave` in Drop. `!Send` keeps both calls on
        // this thread, as `loop_enter`/`loop_leave` require.
        unsafe { mainloop.loop_().enter() };
        Ok(Self {
            mainloop,
            _thread: PhantomData,
        })
    }

    pub(super) fn main_loop(&self) -> &pw::main_loop::MainLoopRc {
        &self.mainloop
    }

    /// Dispatch ready sources and queued invokes for at most `timeout`.
    pub(super) fn iterate(&self, timeout: Duration) -> Result<()> {
        let timeout = pw::loop_::Timeout::Finite(timeout);
        // SAFETY: the loop is entered on this thread for this value's life.
        let result = unsafe { self.mainloop.loop_().iterate_unguarded(timeout) };
        ensure!(
            result >= 0 || result == -libc::EINTR,
            "PipeWire loop failed: {result}"
        );
        Ok(())
    }

    /// Run every invoke already queued for this loop, now, on this thread.
    fn flush(&self) {
        // SAFETY: the loop is live and entered on this thread; an invoke
        // with no function only flushes queued items (`loop_invoke_inthread`).
        unsafe {
            let raw = self.mainloop.loop_().as_raw();
            pw::spa::spa_interface_call_method!(
                raw.loop_,
                pw::spa::sys::spa_loop_methods,
                invoke,
                None,
                0,
                std::ptr::null(),
                0,
                false,
                std::ptr::null_mut()
            );
        }
    }

    /// Own a stream so it is torn down without leaving queued callbacks
    /// that point at it (see [`WorkerStream`]).
    pub(super) fn stream<'a>(&'a self, stream: pw::stream::StreamBox<'a>) -> WorkerStream<'a> {
        WorkerStream {
            stream,
            worker: self,
        }
    }
}

impl Drop for WorkerLoop {
    fn drop(&mut self) {
        // SAFETY: paired with `enter` in `new`, on the same thread (`!Send`).
        unsafe { self.mainloop.loop_().leave() };
    }
}

/// A stream whose loop stays entered: the data thread queues `process`
/// (and drained/trigger) invokes for it instead of running them inline.
/// Disconnecting first stops the node, so nothing new is queued, then the
/// flush runs what is already queued (`do_call_process` skips a
/// disconnecting stream) before PipeWire frees the stream.
///
/// Declare the stream's listener after this value so it drops first: a
/// listener that outlives its stream is a use-after-free in pipewire-rs's
/// `StreamListener::drop` (it unhooks from the freed stream's list), and the
/// flush may still emit drained/trigger callbacks to registered listeners.
pub(super) struct WorkerStream<'a> {
    stream: pw::stream::StreamBox<'a>,
    worker: &'a WorkerLoop,
}

impl<'a> Deref for WorkerStream<'a> {
    type Target = pw::stream::StreamBox<'a>;

    fn deref(&self) -> &Self::Target {
        &self.stream
    }
}

impl Drop for WorkerStream<'_> {
    fn drop(&mut self) {
        // 0 for a stream that never connected, -EBUSY when it's already
        // disconnecting (stream.c 1663-1670); nothing to undo either way.
        let _ = self.stream.disconnect();
        self.worker.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::thread::{self, ThreadId};
    use std::time::Instant;

    /// Runs a real PipeWire playback stream for one second, spending half of
    /// each loop pass outside `iterate` (as workers do between iterations),
    /// and returns the threads its process callback ran on.
    fn callback_threads(entered: bool) -> Vec<ThreadId> {
        thread::spawn(move || {
            pw::init();
            let worker = WorkerLoop::new().unwrap();
            if !entered {
                // The pre-fix shape: iterate() enters and leaves per call.
                // SAFETY: undoes `new`'s enter on this thread; `new`'s Drop
                // re-balances it below.
                unsafe { worker.main_loop().loop_().leave() };
            }
            let context = pw::context::ContextRc::new(worker.main_loop(), None).unwrap();
            let core = context.connect_rc(None).expect("a running PipeWire");
            let stream = worker.stream(
                pw::stream::StreamBox::new(
                    &core,
                    "isocast loop test",
                    pw::properties::properties! {
                        "media.type" => "Audio",
                        "media.category" => "Playback",
                        "media.role" => "Production",
                        "node.name" => "isocast.loop-test",
                        "node.always-process" => "true",
                    },
                )
                .unwrap(),
            );
            let threads = Arc::new(Mutex::new(Vec::new()));
            let seen = threads.clone();
            let listener = stream
                .add_local_listener_with_user_data(())
                .process(move |stream, _| {
                    seen.lock().unwrap().push(thread::current().id());
                    if let Some(mut buffer) = stream.dequeue_buffer() {
                        for data in buffer.datas_mut() {
                            let chunk = data.chunk_mut();
                            *chunk.size_mut() = 0;
                        }
                    }
                })
                .register()
                .unwrap();
            let mut format = pw::spa::param::audio::AudioInfoRaw::new();
            format.set_format(pw::spa::param::audio::AudioFormat::F32LE);
            format.set_rate(48_000);
            format.set_channels(2);
            let bytes = pw::spa::pod::serialize::PodSerializer::serialize(
                std::io::Cursor::new(Vec::new()),
                &pw::spa::pod::Value::Object(pw::spa::pod::Object {
                    type_: pw::spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
                    id: pw::spa::param::ParamType::EnumFormat.as_raw(),
                    properties: format.into(),
                }),
            )
            .unwrap()
            .0
            .into_inner();
            stream
                .connect(
                    pw::spa::utils::Direction::Output,
                    None,
                    pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
                    &mut [pw::spa::pod::Pod::from_bytes(&bytes).unwrap()],
                )
                .unwrap();
            let end = Instant::now() + Duration::from_secs(1);
            while Instant::now() < end {
                if entered {
                    worker.iterate(Duration::from_millis(10)).unwrap();
                } else {
                    worker
                        .main_loop()
                        .loop_()
                        .iterate(pw::loop_::Timeout::Finite(Duration::from_millis(10)));
                }
                thread::sleep(Duration::from_millis(10));
            }
            drop(listener);
            drop(stream);
            if !entered {
                // SAFETY: re-enter so `WorkerLoop`'s Drop leave stays paired.
                unsafe { worker.main_loop().loop_().enter() };
            }
            let worker_thread = thread::current().id();
            let threads = threads.lock().unwrap().clone();
            assert!(
                threads.len() > 10,
                "process ran {} times: needs a PipeWire sink to drive it",
                threads.len()
            );
            threads
                .into_iter()
                .filter(|&id| id != worker_thread)
                .collect()
        })
        .join()
        .unwrap()
    }

    // Needs a running PipeWire with an output sink (the VM fixture, or the
    // hosted Linux capture job): `cargo test -p isocast-ui --
    // --ignored stream_callbacks_run_on_the_worker_thread`.
    #[test]
    #[ignore = "needs a running PipeWire session with a sink"]
    fn stream_callbacks_run_on_the_worker_thread() {
        let fixed = callback_threads(true);
        assert!(
            fixed.is_empty(),
            "{} process callbacks ran off the worker thread",
            fixed.len()
        );
        // Control, by hand only (ISOCAST_PW_LOOP_CONTROL=1): the per-call
        // enter/leave shape this replaced leaks callbacks onto PipeWire's
        // data thread (12-18 per second in the VM). It runs that race,
        // which can corrupt the invoke queue, so CI never runs it.
        if std::env::var_os("ISOCAST_PW_LOOP_CONTROL").is_some() {
            let control = callback_threads(false);
            eprintln!(
                "off-thread process callbacks: entered 0, per-call enter/leave {}",
                control.len()
            );
        }
    }
}
