//! iOS and iPadOS 26: Margin's continued work as a `BGContinuedProcessingTask`
//! (`docs/phase5-brief.md` D1).
//!
//! The [`Coordinator`](super::Coordinator) asks for the task while Margin is in front
//! and a folder this device holds syncs, or a chat that reads one runs. Here that
//! becomes a `BGContinuedProcessingTaskRequest` with `strategy = .fail`: the system runs
//! it now or refuses (then the app behaves as in phase 4: paused in the background,
//! nothing queued for later). While it runs the system shows its title, subtitle and
//! progress, and Margin stays active out of the foreground (`state::set_continued`).
//! When the system expires it, or the owner ends it from the system's UI, the expiration
//! handler clears `continued` (the live loop then closes the socket and pauses the sync,
//! as in phase 4) and completes the task unsuccessfully. `end` completes it successfully.
//!
//! The launch handler is registered once, from the scene delegate as the app launches
//! (continued-processing registrations are exempt from the "before launch finishes"
//! rule, so `start` registers too if that hasn't happened), and runs on the main queue.
//! All task state lives on the main thread; calls from another thread hop to it.
//! Before iOS 26 the classes don't exist: nothing is registered and `start` refuses.

use std::cell::RefCell;
use std::ptr::NonNull;

use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::AnyClass;
use objc2::{AllocAnyThread, MainThreadMarker};
use objc2_background_tasks::{
    BGContinuedProcessingTask, BGContinuedProcessingTaskRequest,
    BGContinuedProcessingTaskRequestSubmissionStrategy, BGTask, BGTaskScheduler,
};
use objc2_foundation::{NSProgressReporting, NSString};

use super::{NoWork, TASK_ID, Work, WorkProgress, WorkSpec};
use crate::state;

/// The platform's [`Work`] on iOS and iPadOS.
#[derive(Debug, Clone, Copy, Default)]
pub struct ContinuedProcessing;

impl Work for ContinuedProcessing {
    fn start(&self, spec: WorkSpec) -> Result<(), String> {
        on_main_sync(move |mtm| start(mtm, spec))
    }

    fn update(&self, progress: WorkProgress) {
        on_main_async(move |_| update(progress));
    }

    fn end(&self) {
        on_main_async(|_| end());
    }
}

/// Registers the launch handler for [`TASK_ID`], once. Call on the main thread while the
/// app launches (the scene delegate does); `false` before iOS 26 or when Info.plist
/// doesn't permit the identifier.
pub fn register(_mtm: MainThreadMarker) -> bool {
    TASK.with_borrow_mut(|task| {
        if let Some(registered) = task.registered {
            return registered;
        }
        let registered = available() && {
            let handler = RcBlock::new(|task: NonNull<BGTask>| launched(task));
            // SAFETY: the main queue has no requirements beyond running on the main
            // thread, which is what `launched` expects; the identifier is a literal.
            unsafe {
                BGTaskScheduler::sharedScheduler()
                    .registerForTaskWithIdentifier_usingQueue_launchHandler(
                        &NSString::from_str(TASK_ID),
                        Some(DispatchQueue::main()),
                        &handler,
                    )
            }
        };
        task.registered = Some(registered);
        registered
    })
}

/// The task's state on the main thread.
#[derive(Default)]
struct Task {
    /// Whether the launch handler is registered (`None`: not tried yet).
    registered: Option<bool>,
    /// A request was accepted and hasn't ended: the next launch is its task.
    submitted: bool,
    /// Counts submissions, so a late expiry doesn't end a newer task.
    generation: u64,
    /// The system's task, once its launch handler ran.
    running: Option<Retained<BGContinuedProcessingTask>>,
    title: String,
    subtitle: String,
    done: u64,
    total: Option<u64>,
}

thread_local! {
    // UIKit, the launch handler (main queue) and the hops below all run on the main thread.
    static TASK: RefCell<Task> = RefCell::default();
}

/// Whether this iOS has continued processing (iOS 26 and later).
fn available() -> bool {
    AnyClass::get(c"BGContinuedProcessingTaskRequest").is_some()
}

fn start(mtm: MainThreadMarker, spec: WorkSpec) -> Result<(), String> {
    if !register(mtm) {
        // Before iOS 26 (or without the plist key): as on the platforms with no
        // continued work.
        return NoWork.start(spec);
    }
    // A task still running from before is done: the coordinator starts a new one only
    // after the last ended, so this only tidies up after an expiry it hasn't seen yet.
    end();
    let subtitle = spec.subtitle.unwrap_or_default();
    // SAFETY: plain initialisers and setters on a request object this thread owns; the
    // scheduler is documented as callable from any thread.
    let submitted = unsafe {
        let request = BGContinuedProcessingTaskRequest::initWithIdentifier_title_subtitle(
            BGContinuedProcessingTaskRequest::alloc(),
            &NSString::from_str(&spec.id),
            &NSString::from_str(&spec.title),
            &NSString::from_str(&subtitle),
        );
        // Never queued: run now or fail, so nothing is silently deferred.
        request.setStrategy(BGContinuedProcessingTaskRequestSubmissionStrategy::Fail);
        BGTaskScheduler::sharedScheduler().submitTaskRequest_error(&request)
    };
    if let Err(error) = submitted {
        return Err(error.localizedDescription().to_string());
    }
    TASK.with_borrow_mut(|task| {
        task.submitted = true;
        task.generation += 1;
        task.title = spec.title;
        task.subtitle = subtitle;
        task.done = 0;
        task.total = None;
    });
    // With `.fail` an accepted request runs now; the launch handler follows at once on
    // the main queue. Active from here, so leaving the app in between doesn't pause.
    state::set_continued(true);
    Ok(())
}

fn update(progress: WorkProgress) {
    TASK.with_borrow_mut(|task| {
        if !task.submitted {
            return;
        }
        task.done = progress.done;
        task.total = progress.total;
        let mut retitled = false;
        if let Some(title) = progress.title {
            retitled |= title != task.title;
            task.title = title;
        }
        if let Some(subtitle) = progress.subtitle {
            retitled |= subtitle != task.subtitle;
            task.subtitle = subtitle;
        }
        if let Some(running) = &task.running {
            show_progress(running, task.done, task.total);
            if retitled {
                // SAFETY: a plain method on the live task, on the main thread.
                unsafe {
                    running.updateTitle_subtitle(
                        &NSString::from_str(&task.title),
                        &NSString::from_str(&task.subtitle),
                    );
                }
            }
        }
    });
}

fn end() {
    let ended = TASK.with_borrow_mut(|task| {
        let was = task.submitted;
        task.submitted = false;
        if let Some(running) = task.running.take() {
            // SAFETY: completing the live task once; the system clears its expiration
            // handler.
            unsafe { running.setTaskCompletedWithSuccess(true) };
        }
        was
    });
    if ended {
        state::set_continued(false);
    }
}

/// The system runs the task (main queue).
fn launched(task: NonNull<BGTask>) {
    // SAFETY: the system hands the launch handler a live task; retaining keeps it.
    let Some(task) = (unsafe { Retained::retain(task.as_ptr()) }) else {
        return;
    };
    let task = match task.downcast::<BGContinuedProcessingTask>() {
        Ok(task) => task,
        Err(other) => {
            // SAFETY: completing a task Margin doesn't use, once.
            unsafe { other.setTaskCompletedWithSuccess(false) };
            return;
        }
    };
    let keep = TASK.with_borrow_mut(|held| {
        if !held.submitted {
            // Ended before the system got to it.
            return false;
        }
        let generation = held.generation;
        let expired = RcBlock::new(move || {
            // The system's time is up or the owner ended the task: phase 4 again.
            state::set_continued(false);
            on_main_async(move |_| expire(generation));
        });
        // SAFETY: the block is 'static and only touches main-thread state through a
        // hop to the main queue; the system clears it once it's called or the task ends.
        unsafe { task.setExpirationHandler(Some(&expired)) };
        show_progress(&task, held.done, held.total);
        // SAFETY: a plain method on the live task, on the main thread.
        unsafe {
            task.updateTitle_subtitle(
                &NSString::from_str(&held.title),
                &NSString::from_str(&held.subtitle),
            );
        }
        held.running = Some(task.clone());
        true
    });
    if keep {
        state::set_continued(true);
    } else {
        // SAFETY: completing the task once.
        unsafe { task.setTaskCompletedWithSuccess(true) };
    }
}

/// The task of `generation` expired (main thread).
fn expire(generation: u64) {
    TASK.with_borrow_mut(|task| {
        if task.generation != generation || !task.submitted {
            return;
        }
        task.submitted = false;
        if let Some(running) = task.running.take() {
            // SAFETY: completing the expired task once, as the system asks.
            unsafe { running.setTaskCompletedWithSuccess(false) };
        }
    });
    state::set_continued(false);
}

/// Bytes as determinate progress; `total: None` as indeterminate.
fn show_progress(task: &BGContinuedProcessingTask, done: u64, total: Option<u64>) {
    let progress = task.progress();
    match total {
        Some(total) => {
            let total = i64::try_from(total).unwrap_or(i64::MAX);
            let done = i64::try_from(done).unwrap_or(i64::MAX).min(total);
            progress.setTotalUnitCount(total);
            progress.setCompletedUnitCount(done);
        }
        None => {
            progress.setTotalUnitCount(-1);
            progress.setCompletedUnitCount(0);
        }
    }
}

/// Runs `f` on the main thread and returns its result: at once when already there,
/// else synchronously on the main queue (the caller is never the main thread then, so
/// this can't deadlock).
fn on_main_sync<R: Send>(f: impl FnOnce(MainThreadMarker) -> R + Send) -> R {
    if let Some(mtm) = MainThreadMarker::new() {
        return f(mtm);
    }
    let mut out = None;
    DispatchQueue::main().exec_sync(|| {
        let mtm = MainThreadMarker::new().expect("the main queue runs on the main thread");
        out = Some(f(mtm));
    });
    out.expect("exec_sync ran the closure")
}

/// Runs `f` on the main thread: at once when already there, else soon on the main queue.
fn on_main_async(f: impl FnOnce(MainThreadMarker) + Send + 'static) {
    if let Some(mtm) = MainThreadMarker::new() {
        f(mtm);
        return;
    }
    DispatchQueue::main().exec_async(move || {
        let mtm = MainThreadMarker::new().expect("the main queue runs on the main thread");
        f(mtm);
    });
}
