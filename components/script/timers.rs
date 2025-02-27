/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::Cell;
use std::cmp::{self, Ord, Ordering};
use std::collections::HashMap;
use std::default::Default;
use std::rc::Rc;

use deny_public_fields::DenyPublicFields;
use euclid::Length;
use ipc_channel::ipc::IpcSender;
use js::jsapi::Heap;
use js::jsval::{JSVal, UndefinedValue};
use js::rust::HandleValue;
use script_traits::{
    precise_time_ms, MsDuration, TimerEvent, TimerEventId, TimerEventRequest, TimerSchedulerMsg,
    TimerSource,
};
use servo_config::pref;

use crate::dom::bindings::callback::ExceptionHandling::Report;
use crate::dom::bindings::cell::DomRefCell;
use crate::dom::bindings::codegen::Bindings::FunctionBinding::Function;
use crate::dom::bindings::reflector::DomObject;
use crate::dom::bindings::str::DOMString;
use crate::dom::document::FakeRequestAnimationFrameCallback;
use crate::dom::eventsource::EventSourceTimeoutCallback;
use crate::dom::globalscope::GlobalScope;
use crate::dom::htmlmetaelement::RefreshRedirectDue;
use crate::dom::testbinding::TestBindingCallback;
use crate::dom::xmlhttprequest::XHRTimeoutCallback;
use crate::script_module::ScriptFetchOptions;
use crate::script_thread::ScriptThread;

#[derive(Clone, Copy, Debug, Eq, Hash, JSTraceable, MallocSizeOf, Ord, PartialEq, PartialOrd)]
pub struct OneshotTimerHandle(i32);

#[derive(DenyPublicFields, JSTraceable, MallocSizeOf)]
pub struct OneshotTimers {
    js_timers: JsTimers,
    #[ignore_malloc_size_of = "Defined in std"]
    #[no_trace]
    /// The sender, to be cloned for each timer,
    /// on which the timer scheduler in the constellation can send an event
    /// when the timer is due.
    timer_event_chan: DomRefCell<Option<IpcSender<TimerEvent>>>,
    #[ignore_malloc_size_of = "Defined in std"]
    #[no_trace]
    /// The sender to the timer scheduler in the constellation.
    scheduler_chan: IpcSender<TimerSchedulerMsg>,
    next_timer_handle: Cell<OneshotTimerHandle>,
    timers: DomRefCell<Vec<OneshotTimer>>,
    #[no_trace]
    suspended_since: Cell<Option<MsDuration>>,
    /// Initially 0, increased whenever the associated document is reactivated
    /// by the amount of ms the document was inactive. The current time can be
    /// offset back by this amount for a coherent time across document
    /// activations.
    #[no_trace]
    suspension_offset: Cell<MsDuration>,
    /// Calls to `fire_timer` with a different argument than this get ignored.
    /// They were previously scheduled and got invalidated when
    ///  - timers were suspended,
    ///  - the timer it was scheduled for got canceled or
    ///  - a timer was added with an earlier callback time. In this case the
    ///    original timer is rescheduled when it is the next one to get called.
    #[no_trace]
    expected_event_id: Cell<TimerEventId>,
}

#[derive(DenyPublicFields, JSTraceable, MallocSizeOf)]
struct OneshotTimer {
    handle: OneshotTimerHandle,
    #[no_trace]
    source: TimerSource,
    callback: OneshotTimerCallback,
    #[no_trace]
    scheduled_for: MsDuration,
}

// This enum is required to work around the fact that trait objects do not support generic methods.
// A replacement trait would have a method such as
//     `invoke<T: DomObject>(self: Box<Self>, this: &T, js_timers: &JsTimers);`.
#[derive(JSTraceable, MallocSizeOf)]
pub enum OneshotTimerCallback {
    XhrTimeout(XHRTimeoutCallback),
    EventSourceTimeout(EventSourceTimeoutCallback),
    JsTimer(JsTimerTask),
    TestBindingCallback(TestBindingCallback),
    FakeRequestAnimationFrame(FakeRequestAnimationFrameCallback),
    RefreshRedirectDue(RefreshRedirectDue),
}

impl OneshotTimerCallback {
    fn invoke<T: DomObject>(self, this: &T, js_timers: &JsTimers) {
        match self {
            OneshotTimerCallback::XhrTimeout(callback) => callback.invoke(),
            OneshotTimerCallback::EventSourceTimeout(callback) => callback.invoke(),
            OneshotTimerCallback::JsTimer(task) => task.invoke(this, js_timers),
            OneshotTimerCallback::TestBindingCallback(callback) => callback.invoke(),
            OneshotTimerCallback::FakeRequestAnimationFrame(callback) => callback.invoke(),
            OneshotTimerCallback::RefreshRedirectDue(callback) => callback.invoke(),
        }
    }
}

impl Ord for OneshotTimer {
    fn cmp(&self, other: &OneshotTimer) -> Ordering {
        match self.scheduled_for.cmp(&other.scheduled_for).reverse() {
            Ordering::Equal => self.handle.cmp(&other.handle).reverse(),
            res => res,
        }
    }
}

impl PartialOrd for OneshotTimer {
    fn partial_cmp(&self, other: &OneshotTimer) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Eq for OneshotTimer {}
impl PartialEq for OneshotTimer {
    fn eq(&self, other: &OneshotTimer) -> bool {
        self as *const OneshotTimer == other as *const OneshotTimer
    }
}

impl OneshotTimers {
    pub fn new(scheduler_chan: IpcSender<TimerSchedulerMsg>) -> OneshotTimers {
        OneshotTimers {
            js_timers: JsTimers::new(),
            timer_event_chan: DomRefCell::new(None),
            scheduler_chan: scheduler_chan,
            next_timer_handle: Cell::new(OneshotTimerHandle(1)),
            timers: DomRefCell::new(Vec::new()),
            suspended_since: Cell::new(None),
            suspension_offset: Cell::new(Length::new(0)),
            expected_event_id: Cell::new(TimerEventId(0)),
        }
    }

    pub fn setup_scheduling(&self, timer_event_chan: IpcSender<TimerEvent>) {
        let mut chan = self.timer_event_chan.borrow_mut();
        assert!(chan.is_none());
        *chan = Some(timer_event_chan);
    }

    pub fn schedule_callback(
        &self,
        callback: OneshotTimerCallback,
        duration: MsDuration,
        source: TimerSource,
    ) -> OneshotTimerHandle {
        let new_handle = self.next_timer_handle.get();
        self.next_timer_handle
            .set(OneshotTimerHandle(new_handle.0 + 1));

        let scheduled_for = self.base_time() + duration;

        let timer = OneshotTimer {
            handle: new_handle,
            source: source,
            callback: callback,
            scheduled_for: scheduled_for,
        };

        {
            let mut timers = self.timers.borrow_mut();
            let insertion_index = timers.binary_search(&timer).err().unwrap();
            timers.insert(insertion_index, timer);
        }

        if self.is_next_timer(new_handle) {
            self.schedule_timer_call();
        }

        new_handle
    }

    pub fn unschedule_callback(&self, handle: OneshotTimerHandle) {
        let was_next = self.is_next_timer(handle);

        self.timers.borrow_mut().retain(|t| t.handle != handle);

        if was_next {
            self.invalidate_expected_event_id();
            self.schedule_timer_call();
        }
    }

    fn is_next_timer(&self, handle: OneshotTimerHandle) -> bool {
        match self.timers.borrow().last() {
            None => false,
            Some(ref max_timer) => max_timer.handle == handle,
        }
    }

    pub fn fire_timer(&self, id: TimerEventId, global: &GlobalScope) {
        let expected_id = self.expected_event_id.get();
        if expected_id != id {
            debug!(
                "ignoring timer fire event {:?} (expected {:?})",
                id, expected_id
            );
            return;
        }

        assert!(self.suspended_since.get().is_none());

        let base_time = self.base_time();

        // Since the event id was the expected one, at least one timer should be due.
        if base_time < self.timers.borrow().last().unwrap().scheduled_for {
            warn!("Unexpected timing!");
            return;
        }

        // select timers to run to prevent firing timers
        // that were installed during fire of another timer
        let mut timers_to_run = Vec::new();

        loop {
            let mut timers = self.timers.borrow_mut();

            if timers.is_empty() || timers.last().unwrap().scheduled_for > base_time {
                break;
            }

            timers_to_run.push(timers.pop().unwrap());
        }

        for timer in timers_to_run {
            // Since timers can be coalesced together inside a task,
            // this loop can keep running, including after an interrupt of the JS,
            // and prevent a clean-shutdown of a JS-running thread.
            // This check prevents such a situation.
            if !global.can_continue_running() {
                return;
            }
            let callback = timer.callback;
            callback.invoke(global, &self.js_timers);
        }

        self.schedule_timer_call();
    }

    fn base_time(&self) -> MsDuration {
        let offset = self.suspension_offset.get();

        match self.suspended_since.get() {
            Some(time) => time - offset,
            None => precise_time_ms() - offset,
        }
    }

    pub fn slow_down(&self) {
        let duration = pref!(js.timers.minimum_duration) as u64;
        self.js_timers.set_min_duration(MsDuration::new(duration));
    }

    pub fn speed_up(&self) {
        self.js_timers.remove_min_duration();
    }

    pub fn suspend(&self) {
        // Suspend is idempotent: do nothing if the timers are already suspended.
        if self.suspended_since.get().is_some() {
            return warn!("Suspending an already suspended timer.");
        }

        debug!("Suspending timers.");
        self.suspended_since.set(Some(precise_time_ms()));
        self.invalidate_expected_event_id();
    }

    pub fn resume(&self) {
        // Resume is idempotent: do nothing if the timers are already resumed.
        let additional_offset = match self.suspended_since.get() {
            Some(suspended_since) => precise_time_ms() - suspended_since,
            None => return warn!("Resuming an already resumed timer."),
        };

        debug!("Resuming timers.");
        self.suspension_offset
            .set(self.suspension_offset.get() + additional_offset);
        self.suspended_since.set(None);

        self.schedule_timer_call();
    }

    fn schedule_timer_call(&self) {
        if self.suspended_since.get().is_some() {
            // The timer will be scheduled when the pipeline is fully activated.
            return;
        }

        let timers = self.timers.borrow();

        if let Some(timer) = timers.last() {
            let expected_event_id = self.invalidate_expected_event_id();

            let delay = Length::new(
                timer
                    .scheduled_for
                    .get()
                    .saturating_sub(precise_time_ms().get()),
            );
            let request = TimerEventRequest(
                self.timer_event_chan
                    .borrow()
                    .clone()
                    .expect("Timer event chan not setup to schedule timers."),
                timer.source,
                expected_event_id,
                delay,
            );
            self.scheduler_chan
                .send(TimerSchedulerMsg(request))
                .unwrap();
        }
    }

    fn invalidate_expected_event_id(&self) -> TimerEventId {
        let TimerEventId(currently_expected) = self.expected_event_id.get();
        let next_id = TimerEventId(currently_expected + 1);
        debug!(
            "invalidating expected timer (was {:?}, now {:?}",
            currently_expected, next_id
        );
        self.expected_event_id.set(next_id);
        next_id
    }

    pub fn set_timeout_or_interval(
        &self,
        global: &GlobalScope,
        callback: TimerCallback,
        arguments: Vec<HandleValue>,
        timeout: i32,
        is_interval: IsInterval,
        source: TimerSource,
    ) -> i32 {
        self.js_timers.set_timeout_or_interval(
            global,
            callback,
            arguments,
            timeout,
            is_interval,
            source,
        )
    }

    pub fn clear_timeout_or_interval(&self, global: &GlobalScope, handle: i32) {
        self.js_timers.clear_timeout_or_interval(global, handle)
    }
}

#[derive(Clone, Copy, Eq, Hash, JSTraceable, MallocSizeOf, Ord, PartialEq, PartialOrd)]
pub struct JsTimerHandle(i32);

#[derive(DenyPublicFields, JSTraceable, MallocSizeOf)]
pub struct JsTimers {
    next_timer_handle: Cell<JsTimerHandle>,
    /// <https://html.spec.whatwg.org/multipage/#list-of-active-timers>
    active_timers: DomRefCell<HashMap<JsTimerHandle, JsTimerEntry>>,
    /// The nesting level of the currently executing timer task or 0.
    nesting_level: Cell<u32>,
    /// Used to introduce a minimum delay in event intervals
    #[no_trace]
    min_duration: Cell<Option<MsDuration>>,
}

#[derive(JSTraceable, MallocSizeOf)]
struct JsTimerEntry {
    oneshot_handle: OneshotTimerHandle,
}

// Holder for the various JS values associated with setTimeout
// (ie. function value to invoke and all arguments to pass
//      to the function when calling it)
// TODO: Handle rooting during invocation when movable GC is turned on
#[derive(JSTraceable, MallocSizeOf)]
pub struct JsTimerTask {
    #[ignore_malloc_size_of = "Because it is non-owning"]
    handle: JsTimerHandle,
    #[no_trace]
    source: TimerSource,
    callback: InternalTimerCallback,
    is_interval: IsInterval,
    nesting_level: u32,
    #[no_trace]
    duration: MsDuration,
    is_user_interacting: bool,
}

// Enum allowing more descriptive values for the is_interval field
#[derive(Clone, Copy, JSTraceable, MallocSizeOf, PartialEq)]
pub enum IsInterval {
    Interval,
    NonInterval,
}

#[derive(Clone)]
pub enum TimerCallback {
    StringTimerCallback(DOMString),
    FunctionTimerCallback(Rc<Function>),
}

#[derive(Clone, JSTraceable, MallocSizeOf)]
enum InternalTimerCallback {
    StringTimerCallback(DOMString),
    FunctionTimerCallback(
        #[ignore_malloc_size_of = "Rc"] Rc<Function>,
        #[ignore_malloc_size_of = "Rc"] Rc<Box<[Heap<JSVal>]>>,
    ),
}

impl JsTimers {
    pub fn new() -> JsTimers {
        JsTimers {
            next_timer_handle: Cell::new(JsTimerHandle(1)),
            active_timers: DomRefCell::new(HashMap::new()),
            nesting_level: Cell::new(0),
            min_duration: Cell::new(None),
        }
    }

    // see https://html.spec.whatwg.org/multipage/#timer-initialisation-steps
    pub fn set_timeout_or_interval(
        &self,
        global: &GlobalScope,
        callback: TimerCallback,
        arguments: Vec<HandleValue>,
        timeout: i32,
        is_interval: IsInterval,
        source: TimerSource,
    ) -> i32 {
        let callback = match callback {
            TimerCallback::StringTimerCallback(code_str) => {
                InternalTimerCallback::StringTimerCallback(code_str)
            },
            TimerCallback::FunctionTimerCallback(function) => {
                // This is a bit complicated, but this ensures that the vector's
                // buffer isn't reallocated (and moved) after setting the Heap values
                let mut args = Vec::with_capacity(arguments.len());
                for _ in 0..arguments.len() {
                    args.push(Heap::default());
                }
                for (i, item) in arguments.iter().enumerate() {
                    args.get_mut(i).unwrap().set(item.get());
                }
                InternalTimerCallback::FunctionTimerCallback(
                    function,
                    Rc::new(args.into_boxed_slice()),
                )
            },
        };

        // step 2
        let JsTimerHandle(new_handle) = self.next_timer_handle.get();
        self.next_timer_handle.set(JsTimerHandle(new_handle + 1));

        // step 3 as part of initialize_and_schedule below

        // step 4
        let mut task = JsTimerTask {
            handle: JsTimerHandle(new_handle),
            source: source,
            callback: callback,
            is_interval: is_interval,
            is_user_interacting: ScriptThread::is_user_interacting(),
            nesting_level: 0,
            duration: Length::new(0),
        };

        // step 5
        task.duration = Length::new(cmp::max(0, timeout) as u64);

        // step 3, 6-9, 11-14
        self.initialize_and_schedule(global, task);

        // step 10
        new_handle
    }

    pub fn clear_timeout_or_interval(&self, global: &GlobalScope, handle: i32) {
        let mut active_timers = self.active_timers.borrow_mut();

        if let Some(entry) = active_timers.remove(&JsTimerHandle(handle)) {
            global.unschedule_callback(entry.oneshot_handle);
        }
    }

    pub fn set_min_duration(&self, duration: MsDuration) {
        self.min_duration.set(Some(duration));
    }

    pub fn remove_min_duration(&self) {
        self.min_duration.set(None);
    }

    // see step 13 of https://html.spec.whatwg.org/multipage/#timer-initialisation-steps
    fn user_agent_pad(&self, current_duration: MsDuration) -> MsDuration {
        match self.min_duration.get() {
            Some(min_duration) => cmp::max(min_duration, current_duration),
            None => current_duration,
        }
    }

    // see https://html.spec.whatwg.org/multipage/#timer-initialisation-steps
    fn initialize_and_schedule(&self, global: &GlobalScope, mut task: JsTimerTask) {
        let handle = task.handle;
        let mut active_timers = self.active_timers.borrow_mut();

        // step 6
        let nesting_level = self.nesting_level.get();

        // step 7, 13
        let duration = self.user_agent_pad(clamp_duration(nesting_level, task.duration));
        // step 8, 9
        task.nesting_level = nesting_level + 1;

        // essentially step 11, 12, and 14
        let callback = OneshotTimerCallback::JsTimer(task);
        let oneshot_handle = global.schedule_callback(callback, duration);

        // step 3
        let entry = active_timers.entry(handle).or_insert(JsTimerEntry {
            oneshot_handle: oneshot_handle,
        });
        entry.oneshot_handle = oneshot_handle;
    }
}

// see step 7 of https://html.spec.whatwg.org/multipage/#timer-initialisation-steps
fn clamp_duration(nesting_level: u32, unclamped: MsDuration) -> MsDuration {
    let lower_bound = if nesting_level > 5 { 4 } else { 0 };

    cmp::max(Length::new(lower_bound), unclamped)
}

impl JsTimerTask {
    // see https://html.spec.whatwg.org/multipage/#timer-initialisation-steps
    pub fn invoke<T: DomObject>(self, this: &T, timers: &JsTimers) {
        // step 4.1 can be ignored, because we proactively prevent execution
        // of this task when its scheduled execution is canceled.

        // prep for step 6 in nested set_timeout_or_interval calls
        timers.nesting_level.set(self.nesting_level);

        // step 4.2
        let was_user_interacting = ScriptThread::is_user_interacting();
        ScriptThread::set_user_interacting(self.is_user_interacting);
        match self.callback {
            InternalTimerCallback::StringTimerCallback(ref code_str) => {
                let global = this.global();
                let cx = GlobalScope::get_cx();
                rooted!(in(*cx) let mut rval = UndefinedValue());
                // FIXME(cybai): Use base url properly by saving private reference for timers (#27260)
                global.evaluate_js_on_global_with_result(
                    code_str,
                    rval.handle_mut(),
                    ScriptFetchOptions::default_classic_script(&global),
                    global.api_base_url(),
                );
            },
            InternalTimerCallback::FunctionTimerCallback(ref function, ref arguments) => {
                let arguments = self.collect_heap_args(arguments);
                let _ = function.Call_(this, arguments, Report);
            },
        };
        ScriptThread::set_user_interacting(was_user_interacting);

        // reset nesting level (see above)
        timers.nesting_level.set(0);

        // step 4.3
        // Since we choose proactively prevent execution (see 4.1 above), we must only
        // reschedule repeating timers when they were not canceled as part of step 4.2.
        if self.is_interval == IsInterval::Interval &&
            timers.active_timers.borrow().contains_key(&self.handle)
        {
            timers.initialize_and_schedule(&this.global(), self);
        }
    }

    // Returning Handles directly from Heap values is inherently unsafe, but here it's
    // always done via rooted JsTimers, which is safe.
    #[allow(unsafe_code)]
    fn collect_heap_args<'b>(&self, args: &'b [Heap<JSVal>]) -> Vec<HandleValue<'b>> {
        args.iter()
            .map(|arg| unsafe { HandleValue::from_raw(arg.handle()) })
            .collect()
    }
}
