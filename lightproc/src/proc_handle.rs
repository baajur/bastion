use std::fmt;
use std::future::Future;
use std::marker::{PhantomData, Unpin};
use std::pin::Pin;
use std::ptr::NonNull;
use std::sync::atomic::Ordering;
use std::task::{Context, Poll};

use crate::state::*;
use crate::utils::abort_on_panic;
use crate::stack::ProcStack;
use crate::proc_data::ProcData;

/// A handle that awaits the result of a task.
///
/// This type is a future that resolves to an `Option<R>` where:
///
/// * `None` indicates the task has panicked or was cancelled
/// * `Some(res)` indicates the task has completed with `res`
pub struct ProcHandle<R> {
    /// A raw task pointer.
    pub(crate) raw_proc: NonNull<()>,

    /// A marker capturing the generic type `R`.
    pub(crate) _private: PhantomData<R>,
}

unsafe impl<R> Send for ProcHandle<R> {}
unsafe impl<R> Sync for ProcHandle<R> {}

impl<R> Unpin for ProcHandle<R> {}

impl<R> ProcHandle<R> {
    /// Cancels the task.
    ///
    /// If the task has already completed, calling this method will have no effect.
    ///
    /// When a task is cancelled, its future cannot be polled again and will be dropped instead.
    pub fn cancel(&self) {
        let ptr = self.raw_proc.as_ptr();
        let header = ptr as *const ProcData;

        unsafe {
            let mut state = (*header).state.load(Ordering::Acquire);

            loop {
                // If the task has been completed or closed, it can't be cancelled.
                if state & (COMPLETED | CLOSED) != 0 {
                    break;
                }

                // If the task is not scheduled nor running, we'll need to schedule it.
                let new = if state & (SCHEDULED | RUNNING) == 0 {
                    (state | SCHEDULED | CLOSED) + REFERENCE
                } else {
                    state | CLOSED
                };

                // Mark the task as closed.
                match (*header).state.compare_exchange_weak(
                    state,
                    new,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => {
                        // If the task is not scheduled nor running, schedule it so that its future
                        // gets dropped by the executor.
                        if state & (SCHEDULED | RUNNING) == 0 {
                            ((*header).vtable.schedule)(ptr);
                        }

                        // Notify the awaiter that the task has been closed.
                        if state & AWAITER != 0 {
                            (*header).notify();
                        }

                        break;
                    }
                    Err(s) => state = s,
                }
            }
        }
    }

    /// Returns a reference to the tag stored inside the task.
    pub fn stack(&self) -> &ProcStack {
        let offset = ProcData::offset_stack();
        let ptr = self.raw_proc.as_ptr();

        unsafe {
            let raw = (ptr as *mut u8).add(offset) as *const ProcStack;
            &*raw
        }
    }
}

impl<R> Drop for ProcHandle<R> {
    fn drop(&mut self) {
        let ptr = self.raw_proc.as_ptr();
        let header = ptr as *const ProcData;

        // A place where the output will be stored in case it needs to be dropped.
        let mut output = None;

        unsafe {
            // Optimistically assume the `ProcHandle` is being dropped just after creating the
            // task. This is a common case so if the handle is not used, the overhead of it is only
            // one compare-exchange operation.
            if let Err(mut state) = (*header).state.compare_exchange_weak(
                SCHEDULED | HANDLE | REFERENCE,
                SCHEDULED | REFERENCE,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                loop {
                    // If the task has been completed but not yet closed, that means its output
                    // must be dropped.
                    if state & COMPLETED != 0 && state & CLOSED == 0 {
                        // Mark the task as closed in order to grab its output.
                        match (*header).state.compare_exchange_weak(
                            state,
                            state | CLOSED,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        ) {
                            Ok(_) => {
                                // Read the output.
                                output =
                                    Some((((*header).vtable.get_output)(ptr) as *mut R).read());

                                // Update the state variable because we're continuing the loop.
                                state |= CLOSED;
                            }
                            Err(s) => state = s,
                        }
                    } else {
                        // If this is the last reference to the task and it's not closed, then
                        // close it and schedule one more time so that its future gets dropped by
                        // the executor.
                        let new = if state & (!(REFERENCE - 1) | CLOSED) == 0 {
                            SCHEDULED | CLOSED | REFERENCE
                        } else {
                            state & !HANDLE
                        };

                        // Unset the handle flag.
                        match (*header).state.compare_exchange_weak(
                            state,
                            new,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        ) {
                            Ok(_) => {
                                // If this is the last reference to the task, we need to either
                                // schedule dropping its future or destroy it.
                                if state & !(REFERENCE - 1) == 0 {
                                    if state & CLOSED == 0 {
                                        ((*header).vtable.schedule)(ptr);
                                    } else {
                                        ((*header).vtable.destroy)(ptr);
                                    }
                                }

                                break;
                            }
                            Err(s) => state = s,
                        }
                    }
                }
            }
        }

        // Drop the output if it was taken out of the task.
        drop(output);
    }
}

impl<R> Future for ProcHandle<R> {
    type Output = Option<R>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let ptr = self.raw_proc.as_ptr();
        let header = ptr as *const ProcData;

        unsafe {
            let mut state = (*header).state.load(Ordering::Acquire);

            loop {
                // If the task has been closed, notify the awaiter and return `None`.
                if state & CLOSED != 0 {
                    // Even though the awaiter is most likely the current task, it could also be
                    // another task.
                    (*header).notify_unless(cx.waker());
                    return Poll::Ready(None);
                }

                // If the task is not completed, register the current task.
                if state & COMPLETED == 0 {
                    // Replace the waker with one associated with the current task. We need a
                    // safeguard against panics because dropping the previous waker can panic.
                    abort_on_panic(|| {
                        (*header).swap_awaiter(Some(cx.waker().clone()));
                    });

                    // Reload the state after registering. It is possible that the task became
                    // completed or closed just before registration so we need to check for that.
                    state = (*header).state.load(Ordering::Acquire);

                    // If the task has been closed, notify the awaiter and return `None`.
                    if state & CLOSED != 0 {
                        // Even though the awaiter is most likely the current task, it could also
                        // be another task.
                        (*header).notify_unless(cx.waker());
                        return Poll::Ready(None);
                    }

                    // If the task is still not completed, we're blocked on it.
                    if state & COMPLETED == 0 {
                        return Poll::Pending;
                    }
                }

                // Since the task is now completed, mark it as closed in order to grab its output.
                match (*header).state.compare_exchange(
                    state,
                    state | CLOSED,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => {
                        // Notify the awaiter. Even though the awaiter is most likely the current
                        // task, it could also be another task.
                        if state & AWAITER != 0 {
                            (*header).notify_unless(cx.waker());
                        }

                        // Take the output from the task.
                        let output = ((*header).vtable.get_output)(ptr) as *mut R;
                        return Poll::Ready(Some(output.read()));
                    }
                    Err(s) => state = s,
                }
            }
        }
    }
}

impl<R> fmt::Debug for ProcHandle<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ptr = self.raw_proc.as_ptr();
        let header = ptr as *const ProcData;

        f.debug_struct("ProcHandle")
            .field("header", unsafe { &(*header) })
            .finish()
    }
}
