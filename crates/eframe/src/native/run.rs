use core::time::Duration;
use std::time::Instant;

use winit::{
    application::ApplicationHandler,
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    window::WindowId,
};

use ahash::HashMap;

use super::winit_integration::{UserEvent, WinitApp};
use crate::{
    Result, epi,
    native::{
        event_loop_context,
        winit_integration::{EventResult, PassMode, is_invisible_or_minimized},
    },
};

/// Minimum interval between consecutive self-requested repaints for invisible windows.
///
/// On Windows, invisible windows don't receive `RedrawRequested` events,
/// so we throttle continuous self-requesting repaint loops while hidden to avoid
/// busy-looping while still running idle wakeups (such as a global hotkey calling
/// `request_repaint` to send `Visible(true)`) promptly.
/// See <https://github.com/emilk/egui/issues/7776>.
const INVISIBLE_WINDOW_REPAINT_INTERVAL: Duration = Duration::from_millis(100);

/// Tracks whether an invisible window's `run_ui_and_paint` pass on the current thread
/// synchronously requested another rapid repaint (`delay < INVISIBLE_WINDOW_REPAINT_INTERVAL`).
///
/// Background-thread wakeups (such as a global hotkey) run on another thread where this
/// slot is `None`, so they never trip the self-requesting hidden loop detector.
#[derive(Clone, Copy)]
struct ActiveInvisiblePass {
    viewport_id: egui::ViewportId,
    requested_rapid_repaint: bool,
}

thread_local! {
    static ACTIVE_INVISIBLE_PASS: core::cell::Cell<Option<ActiveInvisiblePass>> =
        const { core::cell::Cell::new(None) };
}

/// Records a repaint request made on the current thread so [`WinitAppWrapper`] knows
/// whether an invisible pass requested another pass sooner than
/// [`INVISIBLE_WINDOW_REPAINT_INTERVAL`].
pub(crate) fn note_repaint_requested_on_this_thread(
    viewport_id: egui::ViewportId,
    delay: Duration,
) {
    if delay >= INVISIBLE_WINDOW_REPAINT_INTERVAL {
        return;
    }
    ACTIVE_INVISIBLE_PASS.with(|slot| {
        if let Some(mut active) = slot.get()
            && active.viewport_id == viewport_id
        {
            active.requested_rapid_repaint = true;
            slot.set(Some(active));
        }
    });
}

struct InvisiblePassGuard {
    previous: Option<ActiveInvisiblePass>,
    active: bool,
}

impl InvisiblePassGuard {
    fn enter(viewport_id: Option<egui::ViewportId>) -> Self {
        let previous = ACTIVE_INVISIBLE_PASS.with(|slot| {
            slot.replace(viewport_id.map(|viewport_id| ActiveInvisiblePass {
                viewport_id,
                requested_rapid_repaint: false,
            }))
        });
        Self {
            previous,
            active: true,
        }
    }

    fn finish(mut self) -> bool {
        self.active = false;
        let previous = self.previous.take();
        ACTIVE_INVISIBLE_PASS
            .with(|slot| slot.replace(previous))
            .is_some_and(|active| active.requested_rapid_repaint)
    }
}

impl Drop for InvisiblePassGuard {
    fn drop(&mut self) {
        if self.active {
            let previous = self.previous.take();
            ACTIVE_INVISIBLE_PASS.with(|slot| {
                slot.set(previous);
            });
        }
    }
}

/// Per-window throttle state while a window is invisible or minimized.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WindowThrottleState {
    /// Consecutive invisible passes that ended still invisible after requesting a
    /// rapid repaint (`delay < INVISIBLE_WINDOW_REPAINT_INTERVAL`).
    ///
    /// A visible-to-hidden transition can leave `egui`'s `outstanding = 1` settling
    /// pass in flight, which triggers one rapid repaint before `egui` resets
    /// `repaint_delay` to `Duration::MAX`. Letting that first settling pass run
    /// unthrottled lets `egui` reach idle immediately after hiding; a second
    /// consecutive rapid self-request means the app is looping while hidden
    /// (<https://github.com/emilk/egui/issues/7776>) and engages the 100 ms cap.
    consecutive_self_requested: u8,

    /// Earliest instant a throttled repaint is allowed to run.
    next_allowed_repaint: Option<Instant>,
}

/// Tracks hidden-window repaint throttling independently per window.
struct InvisibleWindowThrottles<K> {
    per_window: HashMap<K, WindowThrottleState>,
}

impl<K> Default for InvisibleWindowThrottles<K> {
    fn default() -> Self {
        Self {
            per_window: HashMap::default(),
        }
    }
}

impl<K: Copy + Eq + core::hash::Hash> InvisibleWindowThrottles<K> {
    /// Clears any throttle state once a window becomes visible or disappears.
    fn clear(&mut self, window_id: K) {
        self.per_window.remove(&window_id);
    }

    /// Clamps a `UserEvent::RequestRepaint` timestamp for an invisible window.
    ///
    /// Idle wakeups and unthrottled settling passes keep `when` unchanged.
    /// While throttled, rapid repaints are delayed to `next_allowed_repaint`,
    /// while future-dated `RepaintAt` deadlines (`>= next_allowed_repaint`) are
    /// never pulled earlier.
    fn clamp_user_repaint(&self, window_id: K, when: Instant) -> Instant {
        match self
            .per_window
            .get(&window_id)
            .and_then(|state| state.next_allowed_repaint)
        {
            Some(next_allowed) => when.max(next_allowed),
            None => when,
        }
    }

    /// Updates throttle state after an invisible pass finishes.
    ///
    /// Returns the earliest instant a subsequent rapid repaint may run, or `None`
    /// if the window is not throttled.
    fn finish_invisible_pass(
        &mut self,
        window_id: K,
        now: Instant,
        still_invisible: bool,
        self_requested_rapid_repaint: bool,
    ) -> Option<Instant> {
        if !still_invisible || !self_requested_rapid_repaint {
            self.per_window.remove(&window_id);
            return None;
        }

        let state = self
            .per_window
            .entry(window_id)
            .or_insert(WindowThrottleState {
                consecutive_self_requested: 0,
                next_allowed_repaint: None,
            });
        state.consecutive_self_requested = state.consecutive_self_requested.saturating_add(1);
        if state.consecutive_self_requested >= 2 {
            let next_allowed = now + INVISIBLE_WINDOW_REPAINT_INTERVAL;
            state.next_allowed_repaint = Some(next_allowed);
            Some(next_allowed)
        } else {
            state.next_allowed_repaint = None;
            None
        }
    }
}

fn event_result_requests_rapid_repaint(
    event_result: &Result<EventResult>,
    window_id: WindowId,
    now: Instant,
) -> bool {
    match event_result {
        Ok(EventResult::RepaintNow(id) | EventResult::RepaintNext(id)) => *id == window_id,
        Ok(EventResult::RepaintAt(id, when)) => {
            *id == window_id && *when < now + INVISIBLE_WINDOW_REPAINT_INTERVAL
        }
        _ => false,
    }
}

fn clamp_event_result_for_throttle(
    event_result: Result<EventResult>,
    window_id: WindowId,
    now: Instant,
    next_allowed: Instant,
) -> Result<EventResult> {
    match event_result {
        Ok(EventResult::RepaintNow(id) | EventResult::RepaintNext(id)) if id == window_id => {
            Ok(EventResult::RepaintAt(id, now.max(next_allowed)))
        }
        Ok(EventResult::RepaintAt(id, when)) if id == window_id => {
            Ok(EventResult::RepaintAt(id, when.max(next_allowed)))
        }
        other => other,
    }
}

/// How long to wait for a requested `RedrawRequested` before running the app logic anyway.
///
/// On Wayland, `RedrawRequested` waits for the compositor's frame callback, and compositors
/// may withhold those from a window that isn't shown (e.g. one covered by a fullscreen window),
/// without telling us that it is occluded. Nothing we paint would be shown, and presenting could
/// block, so until the compositor asks for a frame again we only run [`crate::App::logic`].
/// See <https://github.com/emilk/egui/issues/5136>.
const MISSED_REDRAW_TIMEOUT: Duration = Duration::from_millis(250);

// ----------------------------------------------------------------------------
fn create_event_loop(native_options: &mut epi::NativeOptions) -> Result<EventLoop<UserEvent>> {
    #[cfg(target_os = "android")]
    use winit::platform::android::EventLoopBuilderExtAndroid as _;

    profiling::function_scope!();
    let mut builder = winit::event_loop::EventLoop::with_user_event();

    #[cfg(target_os = "android")]
    let mut builder =
        builder.with_android_app(native_options.android_app.take().ok_or_else(|| {
            crate::Error::AppCreation(Box::from(
                "`NativeOptions` is missing required `android_app`",
            ))
        })?);

    if let Some(hook) = core::mem::take(&mut native_options.event_loop_builder) {
        hook(&mut builder);
    }

    profiling::scope!("EventLoopBuilder::build");
    Ok(builder.build()?)
}

/// Access a thread-local event loop.
///
/// We reuse the event-loop so we can support closing and opening an eframe window
/// multiple times. This is just a limitation of winit.
#[cfg(not(target_os = "ios"))]
fn with_event_loop<R>(
    mut native_options: epi::NativeOptions,
    f: impl FnOnce(&mut EventLoop<UserEvent>, epi::NativeOptions) -> R,
) -> Result<R> {
    thread_local!(static EVENT_LOOP: core::cell::RefCell<Option<EventLoop<UserEvent>>> = const { core::cell::RefCell::new(None) });

    EVENT_LOOP.with(|event_loop| {
        // Since we want to reference NativeOptions when creating the EventLoop we can't
        // do that as part of the lazy thread local storage initialization and so we instead
        // create the event loop lazily here
        let mut event_loop_lock = event_loop.borrow_mut();
        let event_loop = if let Some(event_loop) = &mut *event_loop_lock {
            event_loop
        } else {
            event_loop_lock.insert(create_event_loop(&mut native_options)?)
        };
        Ok(f(event_loop, native_options))
    })
}

/// Wraps a [`WinitApp`] to implement [`ApplicationHandler`]. This handles redrawing, exit states, and
/// some events, but otherwise forwards events to the [`WinitApp`].
struct WinitAppWrapper<T: WinitApp> {
    windows_next_repaint_times: HashMap<WindowId, Instant>,

    /// Windows we asked to redraw, and when to give up waiting for their `RedrawRequested`.
    windows_redraw_deadlines: HashMap<WindowId, Instant>,

    /// Per-window throttle state while a window is invisible or minimized.
    invisible_throttles: InvisibleWindowThrottles<WindowId>,

    winit_app: T,
    return_result: Result<(), crate::Error>,
    run_and_return: bool,
}

impl<T: WinitApp> WinitAppWrapper<T> {
    fn new(winit_app: T, run_and_return: bool) -> Self {
        Self {
            windows_next_repaint_times: HashMap::default(),
            windows_redraw_deadlines: HashMap::default(),
            invisible_throttles: InvisibleWindowThrottles::default(),
            winit_app,
            return_result: Ok(()),
            run_and_return,
        }
    }

    fn handle_event_result(
        &mut self,
        event_loop: &ActiveEventLoop,
        event_result: Result<EventResult>,
    ) {
        let mut exit = false;
        let mut save = false;

        log::trace!("event_result: {event_result:?}");

        let mut event_result = event_result;

        if cfg!(target_os = "windows")
            && let Ok(EventResult::RepaintNow(window_id)) = event_result
        {
            log::trace!("RepaintNow of {window_id:?}");
            self.windows_next_repaint_times
                .insert(window_id, Instant::now());

            // Fix flickering on Windows, see https://github.com/emilk/egui/pull/2280
            event_result = self
                .winit_app
                .run_ui_and_paint(event_loop, window_id, PassMode::Full);
        }

        let combined_result = event_result.map(|event_result| match event_result {
            EventResult::Wait => {
                event_loop.set_control_flow(ControlFlow::Wait);
                event_result
            }
            EventResult::RepaintNow(window_id) => {
                log::trace!("RepaintNow of {window_id:?}");
                self.windows_next_repaint_times
                    .insert(window_id, Instant::now());
                event_result
            }
            EventResult::RepaintNext(window_id) => {
                log::trace!("RepaintNext of {window_id:?}");
                self.windows_next_repaint_times
                    .insert(window_id, Instant::now());
                event_result
            }
            EventResult::RepaintAt(window_id, repaint_time) => {
                self.windows_next_repaint_times.insert(
                    window_id,
                    self.windows_next_repaint_times
                        .get(&window_id)
                        .map_or(repaint_time, |last| (*last).min(repaint_time)),
                );
                event_result
            }
            EventResult::Save => {
                save = true;
                event_result
            }
            EventResult::Exit => {
                exit = true;
                event_result
            }
            EventResult::CloseRequested => {
                // The windows need to be dropped whilst the event loop is running to allow for proper cleanup.
                self.winit_app.save_and_destroy();
                event_result
            }
        });

        if let Err(err) = combined_result {
            log::error!("Exiting because of error: {err}");
            exit = true;
            self.return_result = Err(err);
        }

        if save {
            log::debug!("Received an EventResult::Save - saving app state");
            self.winit_app.save();
        }

        if exit {
            if self.run_and_return {
                log::debug!("Asking to exit event loop…");
                event_loop.exit();
            } else {
                log::debug!("Quitting - saving app state…");
                self.winit_app.save_and_destroy();

                log::debug!("Exiting with return code 0");

                std::process::exit(0);
            }
        }

        self.check_redraw_requests(event_loop);
    }

    fn check_redraw_requests(&mut self, event_loop: &ActiveEventLoop) {
        let now = Instant::now();

        let mut invisible_window_ids = Vec::new();

        self.windows_next_repaint_times
            .retain(|window_id, repaint_time| {
                if now < *repaint_time {
                    return true; // not yet ready
                }

                if let Some(window) = self.winit_app.window(*window_id) {
                    // On Windows, invisible windows don't receive RedrawRequested
                    // events, so pending viewport commands (e.g. Visible(true)) would
                    // never be processed. We collect these windows to paint them
                    // directly below.
                    // See: https://github.com/emilk/egui/issues/5229
                    if is_invisible_or_minimized(&window) {
                        invisible_window_ids.push(*window_id);
                    } else {
                        self.invisible_throttles.clear(*window_id);
                        log::trace!("request_redraw for {window_id:?}");
                        // Don't switch to `ControlFlow::Poll` here. `request_redraw`
                        // is enough to wake the event loop, and on Wayland the
                        // `RedrawRequested` event is only delivered once the
                        // compositor sends a frame callback. Polling in the meantime
                        // busy-loops a whole CPU core.
                        // See https://github.com/emilk/egui/issues/8326.
                        window.request_redraw();
                        self.windows_redraw_deadlines
                            .entry(*window_id)
                            .or_insert(now + MISSED_REDRAW_TIMEOUT);
                    }
                } else {
                    self.invisible_throttles.clear(*window_id);
                    log::trace!("No window found for {window_id:?}");
                }
                false
            });

        // Paint invisible windows directly, since they won't receive
        // RedrawRequested events on Windows. This ensures that viewport
        // commands like Visible(true) are still processed.
        for window_id in &invisible_window_ids {
            let viewport_id = self.winit_app.viewport_id_from_window_id(*window_id);
            let pass_guard = InvisiblePassGuard::enter(viewport_id);
            let event_result =
                self.winit_app
                    .run_ui_and_paint(event_loop, *window_id, PassMode::Full);
            let requested_via_callback = pass_guard.finish();

            let pass_end = Instant::now();
            let still_invisible = self
                .winit_app
                .window(*window_id)
                .is_some_and(|window| is_invisible_or_minimized(&window));
            let self_requested_rapid = requested_via_callback
                || event_result_requests_rapid_repaint(&event_result, *window_id, pass_end);
            let next_allowed = self.invisible_throttles.finish_invisible_pass(
                *window_id,
                pass_end,
                still_invisible,
                self_requested_rapid,
            );

            let event_result = if let Some(next_allowed) = next_allowed {
                clamp_event_result_for_throttle(event_result, *window_id, pass_end, next_allowed)
            } else {
                event_result
            };
            self.handle_event_result(event_loop, event_result);

            if let Some(next_allowed) = next_allowed {
                self.windows_next_repaint_times
                    .entry(*window_id)
                    .and_modify(|t| *t = (*t).max(next_allowed));
            }
        }

        // Keep the app logic running for windows whose `RedrawRequested` is overdue,
        // e.g. because the compositor withholds frame callbacks from a hidden window.
        let overdue_window_ids: Vec<WindowId> = self
            .windows_redraw_deadlines
            .iter()
            .filter(|(_, deadline)| **deadline <= now)
            .map(|(window_id, _)| *window_id)
            .collect();
        for window_id in overdue_window_ids {
            // `handle_event_result` re-enters `check_redraw_requests`, which may already
            // have handled this window, so only run it if the deadline is still there:
            if self.windows_redraw_deadlines.remove(&window_id).is_some() {
                log::trace!("RedrawRequested is overdue for {window_id:?}: running logic only");
                let event_result =
                    self.winit_app
                        .run_ui_and_paint(event_loop, window_id, PassMode::LogicOnly);
                self.handle_event_result(event_loop, event_result);
            }
        }

        // Always set an explicit, sleeping control flow. Previously we only set
        // `WaitUntil` when a repaint was already scheduled, which meant that a
        // `ControlFlow::Poll` set earlier was never undone once the last timed
        // repaint had been consumed, leaving the loop spinning.
        // See https://github.com/emilk/egui/issues/8326.
        let next_repaint_time = core::iter::chain(
            self.windows_next_repaint_times.values(),
            self.windows_redraw_deadlines.values(),
        )
        .min()
        .copied();
        event_loop.set_control_flow(match next_repaint_time {
            Some(next_repaint_time) => ControlFlow::WaitUntil(next_repaint_time),
            None => ControlFlow::Wait,
        });
    }
}

impl<T: WinitApp> ApplicationHandler<UserEvent> for WinitAppWrapper<T> {
    fn suspended(&mut self, event_loop: &ActiveEventLoop) {
        profiling::scope!("Event::Suspended");

        event_loop_context::with_event_loop_context(event_loop, move || {
            let event_result = self.winit_app.suspended(event_loop);
            self.handle_event_result(event_loop, event_result);
        });
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        profiling::scope!("Event::Resumed");

        // Nb: Make sure this guard is dropped after this function returns.
        event_loop_context::with_event_loop_context(event_loop, move || {
            let event_result = self.winit_app.resumed(event_loop);
            self.handle_event_result(event_loop, event_result);
        });
    }

    fn exiting(&mut self, event_loop: &ActiveEventLoop) {
        // On Mac, Cmd-Q we get here and then `run_app_on_demand` doesn't return (despite its name),
        // so we need to save state now:
        log::debug!("Received Event::LoopExiting - saving app state…");
        event_loop_context::with_event_loop_context(event_loop, move || {
            self.winit_app.save_and_destroy();
        });
    }

    fn device_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        device_id: winit::event::DeviceId,
        event: winit::event::DeviceEvent,
    ) {
        profiling::function_scope!(egui_winit::short_device_event_description(&event));

        // Nb: Make sure this guard is dropped after this function returns.
        event_loop_context::with_event_loop_context(event_loop, move || {
            let event_result = self.winit_app.device_event(event_loop, device_id, event);
            self.handle_event_result(event_loop, event_result);
        });
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        profiling::function_scope!(match &event {
            UserEvent::RequestRepaint { .. } => "UserEvent::RequestRepaint",
            #[cfg(feature = "accesskit")]
            UserEvent::AccessKitActionRequest(_) => "UserEvent::AccessKitActionRequest",
        });

        event_loop_context::with_event_loop_context(event_loop, move || {
            let event_result = match event {
                UserEvent::RequestRepaint {
                    when,
                    cumulative_pass_nr,
                    viewport_id,
                } => {
                    let current_pass_nr = self
                        .winit_app
                        .egui_ctx()
                        .map_or(0, |ctx| ctx.cumulative_pass_nr_for(viewport_id));
                    if current_pass_nr == cumulative_pass_nr
                        || current_pass_nr == cumulative_pass_nr + 1
                    {
                        log::trace!("UserEvent::RequestRepaint scheduling repaint at {when:?}");
                        if let Some(window_id) =
                            self.winit_app.window_id_from_viewport_id(viewport_id)
                        {
                            // Throttle only sustained self-requesting hidden repaint loops
                            // (emilk/egui#7776) without delaying idle external wakeups or
                            // pulling future-dated deadlines earlier.
                            let when = if let Some(window) = self.winit_app.window(window_id)
                                && is_invisible_or_minimized(&window)
                            {
                                self.invisible_throttles.clamp_user_repaint(window_id, when)
                            } else {
                                self.invisible_throttles.clear(window_id);
                                when
                            };
                            Ok(EventResult::RepaintAt(window_id, when))
                        } else {
                            Ok(EventResult::Wait)
                        }
                    } else {
                        log::trace!("Got outdated UserEvent::RequestRepaint");
                        Ok(EventResult::Wait) // old request - we've already repainted
                    }
                }
                #[cfg(feature = "accesskit")]
                UserEvent::AccessKitActionRequest(request) => {
                    self.winit_app.on_accesskit_event(request)
                }
            };
            self.handle_event_result(event_loop, event_result);
        });
    }

    fn new_events(&mut self, event_loop: &ActiveEventLoop, cause: winit::event::StartCause) {
        if let winit::event::StartCause::ResumeTimeReached { .. } = cause {
            log::trace!("Woke up to check next_repaint_time");
        }

        self.check_redraw_requests(event_loop);
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: winit::event::WindowEvent,
    ) {
        profiling::function_scope!(egui_winit::short_window_event_description(&event));

        // Nb: Make sure this guard is dropped after this function returns.
        event_loop_context::with_event_loop_context(event_loop, move || {
            if self
                .winit_app
                .window(window_id)
                .is_none_or(|window| !is_invisible_or_minimized(&window))
            {
                self.invisible_throttles.clear(window_id);
            }

            let event_result = if event == winit::event::WindowEvent::RedrawRequested {
                self.windows_redraw_deadlines.remove(&window_id);
                self.winit_app
                    .run_ui_and_paint(event_loop, window_id, PassMode::Full)
            } else {
                self.winit_app.window_event(event_loop, window_id, event)
            };

            self.handle_event_result(event_loop, event_result);
        });
    }
}

#[cfg(not(target_os = "ios"))]
fn run_and_return(event_loop: &mut EventLoop<UserEvent>, winit_app: impl WinitApp) -> Result {
    use winit::platform::run_on_demand::EventLoopExtRunOnDemand as _;

    log::trace!("Entering the winit event loop (run_app_on_demand)…");

    let mut app = WinitAppWrapper::new(winit_app, true);
    event_loop.run_app_on_demand(&mut app)?;
    log::debug!("eframe window closed");
    app.return_result
}

fn run_and_exit(event_loop: EventLoop<UserEvent>, winit_app: impl WinitApp) -> Result {
    log::trace!("Entering the winit event loop (run_app)…");

    // When to repaint what window
    let mut app = WinitAppWrapper::new(winit_app, false);
    event_loop.run_app(&mut app)?;

    log::debug!("winit event loop unexpectedly returned");
    Ok(())
}

// ----------------------------------------------------------------------------

#[cfg(feature = "glow")]
pub fn run_glow(
    app_name: &str,
    mut native_options: epi::NativeOptions,
    egui_ctx: Option<egui::Context>,
    app_creator: epi::AppCreator<'_>,
) -> Result {
    use super::glow_integration::GlowWinitApp;

    #[cfg(not(target_os = "ios"))]
    if native_options.run_and_return {
        return with_event_loop(native_options, |event_loop, native_options| {
            let glow_eframe =
                GlowWinitApp::new(event_loop, app_name, native_options, egui_ctx, app_creator);
            run_and_return(event_loop, glow_eframe)
        })?;
    }

    let event_loop = create_event_loop(&mut native_options)?;
    let glow_eframe =
        GlowWinitApp::new(&event_loop, app_name, native_options, egui_ctx, app_creator);
    run_and_exit(event_loop, glow_eframe)
}

#[cfg(feature = "glow")]
pub fn create_glow<'a>(
    app_name: &str,
    native_options: epi::NativeOptions,
    app_creator: epi::AppCreator<'a>,
    event_loop: &EventLoop<UserEvent>,
) -> impl ApplicationHandler<UserEvent> + 'a {
    use super::glow_integration::GlowWinitApp;

    let glow_eframe = GlowWinitApp::new(event_loop, app_name, native_options, None, app_creator);
    WinitAppWrapper::new(glow_eframe, true)
}

// ----------------------------------------------------------------------------

#[cfg(feature = "wgpu_no_default_features")]
pub fn run_wgpu(
    app_name: &str,
    mut native_options: epi::NativeOptions,
    egui_ctx: Option<egui::Context>,
    app_creator: epi::AppCreator<'_>,
) -> Result {
    use super::wgpu_integration::WgpuWinitApp;

    #[cfg(not(target_os = "ios"))]
    if native_options.run_and_return {
        return with_event_loop(native_options, |event_loop, native_options| {
            let wgpu_eframe =
                WgpuWinitApp::new(event_loop, app_name, native_options, egui_ctx, app_creator);
            run_and_return(event_loop, wgpu_eframe)
        })?;
    }

    let event_loop = create_event_loop(&mut native_options)?;
    let wgpu_eframe =
        WgpuWinitApp::new(&event_loop, app_name, native_options, egui_ctx, app_creator);
    run_and_exit(event_loop, wgpu_eframe)
}

#[cfg(feature = "wgpu_no_default_features")]
pub fn create_wgpu<'a>(
    app_name: &str,
    native_options: epi::NativeOptions,
    app_creator: epi::AppCreator<'a>,
    event_loop: &EventLoop<UserEvent>,
) -> impl ApplicationHandler<UserEvent> + 'a {
    use super::wgpu_integration::WgpuWinitApp;

    let wgpu_eframe = WgpuWinitApp::new(event_loop, app_name, native_options, None, app_creator);
    WinitAppWrapper::new(wgpu_eframe, true)
}

// ----------------------------------------------------------------------------

/// A proxy to the eframe application that implements [`ApplicationHandler`].
///
/// This can be run directly on your own [`EventLoop`] by itself or with other
/// windows you manage outside of eframe.
pub struct EframeWinitApplication<'a> {
    wrapper: Box<dyn ApplicationHandler<UserEvent> + 'a>,
    control_flow: ControlFlow,
}

impl ApplicationHandler<UserEvent> for EframeWinitApplication<'_> {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        self.wrapper.resumed(event_loop);
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: winit::window::WindowId,
        event: winit::event::WindowEvent,
    ) {
        self.wrapper.window_event(event_loop, window_id, event);
    }

    fn new_events(&mut self, event_loop: &ActiveEventLoop, cause: winit::event::StartCause) {
        self.wrapper.new_events(event_loop, cause);
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        self.wrapper.user_event(event_loop, event);
    }

    fn device_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        device_id: winit::event::DeviceId,
        event: winit::event::DeviceEvent,
    ) {
        self.wrapper.device_event(event_loop, device_id, event);
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.wrapper.about_to_wait(event_loop);
        self.control_flow = event_loop.control_flow();
    }

    fn suspended(&mut self, event_loop: &ActiveEventLoop) {
        self.wrapper.suspended(event_loop);
    }

    fn exiting(&mut self, event_loop: &ActiveEventLoop) {
        self.wrapper.exiting(event_loop);
    }

    fn memory_warning(&mut self, event_loop: &ActiveEventLoop) {
        self.wrapper.memory_warning(event_loop);
    }
}

impl<'a> EframeWinitApplication<'a> {
    pub(crate) fn new<T: ApplicationHandler<UserEvent> + 'a>(app: T) -> Self {
        Self {
            wrapper: Box::new(app),
            control_flow: ControlFlow::default(),
        }
    }

    /// Pump the `EventLoop` to check for and dispatch pending events to this application.
    ///
    /// Returns either the exit code for the application or the final state of the [`ControlFlow`]
    /// after all events have been dispatched in this iteration.
    ///
    /// This is useful when your [`EventLoop`] is not the main event loop for your application.
    /// See the `external_eventloop_async` example.
    #[cfg(not(target_os = "ios"))]
    pub fn pump_eframe_app(
        &mut self,
        event_loop: &mut EventLoop<UserEvent>,
        timeout: Option<core::time::Duration>,
    ) -> EframePumpStatus {
        use winit::platform::pump_events::{EventLoopExtPumpEvents as _, PumpStatus};

        match event_loop.pump_app_events(timeout, self) {
            PumpStatus::Continue => EframePumpStatus::Continue(self.control_flow),
            PumpStatus::Exit(code) => EframePumpStatus::Exit(code),
        }
    }
}

/// Either an exit code or a [`ControlFlow`] from the [`ActiveEventLoop`].
///
/// The result of [`EframeWinitApplication::pump_eframe_app`].
#[cfg(not(target_os = "ios"))]
pub enum EframePumpStatus {
    /// The final state of the [`ControlFlow`] after all events have been dispatched
    ///
    /// Callers should perform the action that is appropriate for the [`ControlFlow`] value.
    Continue(ControlFlow),

    /// The exit code for the application
    Exit(i32),
}

#[cfg(test)]
mod tests {
    use core::time::Duration;
    use std::{sync::Arc, time::Instant};

    use egui::{RawInput, ViewportId, mutex::Mutex};

    use super::{
        INVISIBLE_WINDOW_REPAINT_INTERVAL, InvisiblePassGuard, InvisibleWindowThrottles,
        clamp_event_result_for_throttle, event_result_requests_rapid_repaint,
        note_repaint_requested_on_this_thread,
    };
    use crate::native::winit_integration::EventResult;

    fn wired_context() -> (egui::Context, Arc<Mutex<Vec<(ViewportId, Duration)>>>) {
        let ctx = egui::Context::default();
        let events = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&events);
        ctx.set_request_repaint_callback(move |info| {
            note_repaint_requested_on_this_thread(info.viewport_id, info.delay);
            recorded.lock().push((info.viewport_id, info.delay));
        });
        (ctx, events)
    }

    #[test]
    fn idle_hidden_wakeup_runs_immediately() {
        let mut throttles = InvisibleWindowThrottles::default();
        let window_id = 1_u64;
        let t0 = Instant::now();

        let scheduled = throttles.clamp_user_repaint(window_id, t0);
        assert_eq!(
            scheduled, t0,
            "idle hidden window must not wait for the 100 ms throttle"
        );

        let next_allowed = throttles.finish_invisible_pass(window_id, t0, false, true);
        assert_eq!(
            next_allowed, None,
            "pass that made the window visible must not engage a hidden throttle"
        );
    }

    #[test]
    fn post_dismissal_settling_and_immediate_resummon_run_unthrottled() {
        let (ctx, events) = wired_context();
        let mut throttles = InvisibleWindowThrottles::default();
        let window_id = 1_u64;
        let t0 = Instant::now();

        // Visible dismissal pass where App::ui calls request_repaint(), leaving outstanding = 1.
        ctx.run_ui(RawInput::default(), |ui| {
            ui.ctx().request_repaint();
        })
        .drop_without_applying_deltas();
        events.lock().clear();

        // Hidden pass 1: egui drains outstanding 1 -> 0 and emits one settling repaint.
        let guard = InvisiblePassGuard::enter(Some(ViewportId::ROOT));
        let _ = ctx.run_logic(&RawInput::default(), |_ctx| {});
        let requested_1 = guard.finish();
        assert!(
            requested_1,
            "draining outstanding = 1 during pass 1 emits a settling rapid repaint"
        );
        assert_eq!(
            throttles.finish_invisible_pass(window_id, t0, true, requested_1),
            None,
            "first settling pass after hide must remain unthrottled"
        );

        // Settling repaint arrives immediately while hidden.
        let t1 = t0 + Duration::from_millis(1);
        assert_eq!(
            throttles.clamp_user_repaint(window_id, t1),
            t1,
            "settling repaint must not be delayed by 100 ms"
        );

        // Hidden pass 2: outstanding is 0, so egui resets repaint_delay to Duration::MAX.
        events.lock().clear();
        let guard = InvisiblePassGuard::enter(Some(ViewportId::ROOT));
        let _ = ctx.run_logic(&RawInput::default(), |_ctx| {});
        let requested_2 = guard.finish();
        assert!(
            !requested_2,
            "settled hidden pass must emit no further rapid repaint"
        );
        assert_eq!(
            throttles.finish_invisible_pass(window_id, t1, true, requested_2),
            None,
            "settled hidden pass must reset throttle state to idle"
        );

        // Immediate re-summon 1 ms later fires a real callback and runs without delay.
        ctx.request_repaint();
        let fired = events.lock().len();
        assert_eq!(
            fired, 1,
            "settled hidden context must forward an immediate re-summon request_repaint"
        );
        let t2 = t1 + Duration::from_millis(1);
        assert_eq!(
            throttles.clamp_user_repaint(window_id, t2),
            t2,
            "immediate re-summon right after settling must not be throttled"
        );
    }

    #[test]
    fn window_events_and_future_timers_do_not_throttle_resummon() {
        let (ctx, _events) = wired_context();
        let mut throttles = InvisibleWindowThrottles::default();
        let window_id = 1_u64;
        let t0 = Instant::now();

        // Drain initial default outstanding = 1 while visible.
        ctx.run_ui(RawInput::default(), |_ui| {})
            .drop_without_applying_deltas();
        ctx.run_ui(RawInput::default(), |_ui| {})
            .drop_without_applying_deltas();

        // Simulate Focused(false), a pre-hide timer scheduling a 5 s future timer, and Occluded(true).
        for step in 0_u64..3 {
            let guard = InvisiblePassGuard::enter(Some(ViewportId::ROOT));
            let _ = ctx.run_logic(&RawInput::default(), |ctx| {
                if step == 1 {
                    ctx.request_repaint_after(Duration::from_secs(5));
                }
            });
            let requested_rapid = guard.finish();
            assert!(
                !requested_rapid,
                "idle or >= 100 ms timer hidden passes must not count as rapid self-requests"
            );
            assert_eq!(
                throttles.finish_invisible_pass(
                    window_id,
                    t0 + Duration::from_millis(step * 10),
                    true,
                    requested_rapid,
                ),
                None,
                "non-looping hidden passes must leave throttle unengaged"
            );
        }

        let resummon_at = t0 + Duration::from_millis(25);
        assert_eq!(
            throttles.clamp_user_repaint(window_id, resummon_at),
            resummon_at,
            "re-summon after Focused(false)/Occluded(true)/future timer must run immediately"
        );
    }

    #[test]
    fn continuous_self_requesting_hidden_loop_is_throttled() {
        let (ctx, _events) = wired_context();
        let mut throttles = InvisibleWindowThrottles::default();
        let window_id = 1_u64;
        let t0 = Instant::now();

        // Pass 1: app self-requests immediate repaint while hidden.
        let guard = InvisiblePassGuard::enter(Some(ViewportId::ROOT));
        let _ = ctx.run_logic(&RawInput::default(), |ctx| {
            ctx.request_repaint();
        });
        assert!(
            guard.finish(),
            "in-pass request_repaint must be detected as a rapid self-request"
        );
        assert_eq!(
            throttles.finish_invisible_pass(window_id, t0, true, true),
            None,
            "first rapid pass stays unthrottled to allow settling"
        );

        // Pass 2: app self-requests again -> engages 100 ms throttle.
        let t1 = t0 + Duration::from_millis(1);
        let guard = InvisiblePassGuard::enter(Some(ViewportId::ROOT));
        let _ = ctx.run_logic(&RawInput::default(), |ctx| {
            ctx.request_repaint();
        });
        assert!(
            guard.finish(),
            "second consecutive self-requesting pass must be detected"
        );
        let expected_next = t1 + INVISIBLE_WINDOW_REPAINT_INTERVAL;
        assert_eq!(
            throttles.finish_invisible_pass(window_id, t1, true, true),
            Some(expected_next),
            "second consecutive self-requesting hidden pass must engage 100 ms throttle"
        );

        // Subsequent immediate UserEvent::RequestRepaint is held until t1 + 100 ms.
        assert_eq!(
            throttles.clamp_user_repaint(window_id, t1),
            expected_next,
            "continuous hidden loop must be capped to INVISIBLE_WINDOW_REPAINT_INTERVAL"
        );

        // Short-delay loop (5 ms) is also detected and stays throttled.
        let t2 = expected_next;
        let guard = InvisiblePassGuard::enter(Some(ViewportId::ROOT));
        let _ = ctx.run_logic(&RawInput::default(), |ctx| {
            ctx.request_repaint_after(Duration::from_millis(5));
        });
        assert!(
            guard.finish(),
            "in-pass 5 ms request_repaint_after must count as a rapid self-request"
        );
        let expected_next_2 = t2 + INVISIBLE_WINDOW_REPAINT_INTERVAL;
        assert_eq!(
            throttles.finish_invisible_pass(window_id, t2, true, true),
            Some(expected_next_2),
            "continued loop must advance the 100 ms throttle window"
        );
        assert_eq!(
            throttles.clamp_user_repaint(window_id, t2 + Duration::from_millis(5)),
            expected_next_2,
            "5 ms hidden loop must be clamped to 100 ms"
        );
    }

    #[test]
    fn future_repaint_at_deadline_is_never_pulled_earlier() {
        let mut throttles = InvisibleWindowThrottles::default();
        let window_id = winit::window::WindowId::dummy();
        let t0 = Instant::now();

        let _ = throttles.finish_invisible_pass(window_id, t0, true, true);
        let next_allowed = throttles
            .finish_invisible_pass(window_id, t0, true, true)
            .expect("second self-requested pass must engage throttle");

        let future = t0 + Duration::from_secs(5);
        assert_eq!(
            throttles.clamp_user_repaint(window_id, future),
            future,
            "future UserEvent::RequestRepaint must never be pulled earlier"
        );

        assert!(
            !event_result_requests_rapid_repaint(
                &Ok(EventResult::RepaintAt(window_id, future)),
                window_id,
                t0,
            ),
            "5 s RepaintAt must not count as a rapid repaint"
        );

        let clamped = clamp_event_result_for_throttle(
            Ok(EventResult::RepaintAt(window_id, future)),
            window_id,
            t0,
            next_allowed,
        );
        assert!(
            matches!(clamped, Ok(EventResult::RepaintAt(id, when)) if id == window_id && when == future),
            "future RepaintAt EventResult must never be pulled earlier"
        );

        let immediate = clamp_event_result_for_throttle(
            Ok(EventResult::RepaintNext(window_id)),
            window_id,
            t0,
            next_allowed,
        );
        assert!(
            matches!(immediate, Ok(EventResult::RepaintAt(id, when)) if id == window_id && when == next_allowed),
            "RepaintNext during a throttled loop must be pushed out to next_allowed"
        );
    }

    #[test]
    fn visibility_transition_clears_hidden_throttle_immediately() {
        let mut throttles = InvisibleWindowThrottles::default();
        let window_id = 1_u64;
        let t0 = Instant::now();

        let _ = throttles.finish_invisible_pass(window_id, t0, true, true);
        assert!(
            throttles
                .finish_invisible_pass(window_id, t0, true, true)
                .is_some(),
            "looping hidden window should be throttled"
        );

        // Becoming visible during a pass clears the throttle immediately.
        let t1 = t0 + Duration::from_millis(5);
        assert_eq!(
            throttles.finish_invisible_pass(window_id, t1, false, true),
            None,
            "becoming visible during a pass must clear throttle"
        );
        assert_eq!(
            throttles.clamp_user_repaint(window_id, t1),
            t1,
            "subsequent repaint after becoming visible must not be throttled"
        );

        // Re-engage throttle, then simulate OS unminimize clearing it directly.
        let _ = throttles.finish_invisible_pass(window_id, t1, true, true);
        assert!(
            throttles
                .finish_invisible_pass(window_id, t1, true, true)
                .is_some(),
            "throttle should re-engage on a new hidden loop"
        );
        throttles.clear(window_id);
        let t2 = t1 + Duration::from_millis(5);
        assert_eq!(
            throttles.clamp_user_repaint(window_id, t2),
            t2,
            "external visibility transition must clear throttle immediately"
        );
    }

    #[test]
    fn background_thread_and_other_viewport_repaints_do_not_trip_loop_detector() {
        let guard = InvisiblePassGuard::enter(Some(ViewportId::ROOT));

        // Another viewport requesting a rapid repaint on the main thread does not count.
        note_repaint_requested_on_this_thread(ViewportId::from_hash_of("child"), Duration::ZERO);

        // A background thread (e.g. global hotkey) requesting a repaint for ROOT does not count.
        let handle = std::thread::Builder::new()
            .name("hotkey-wakeup".to_owned())
            .spawn(|| {
                note_repaint_requested_on_this_thread(ViewportId::ROOT, Duration::ZERO);
            })
            .expect("background thread should spawn");
        assert!(
            handle.join().is_ok(),
            "background thread should complete cleanly"
        );

        assert!(
            !guard.finish(),
            "background-thread and other-viewport repaints must not trip the main-thread detector"
        );
    }
}
