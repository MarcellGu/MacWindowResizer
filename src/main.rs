//! Native window creation callbacks with config.toml embedded at compile time.

use block2::RcBlock;
use objc2::{MainThreadMarker, Message, rc::Retained, runtime::ProtocolObject};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSRunningApplication, NSScreen, NSWorkspace,
    NSWorkspaceApplicationKey, NSWorkspaceDidActivateApplicationNotification,
    NSWorkspaceDidLaunchApplicationNotification, NSWorkspaceDidTerminateApplicationNotification,
};
use objc2_application_services::{
    AXError, AXIsProcessTrusted, AXIsProcessTrustedWithOptions, AXObserver, AXUIElement, AXValue,
    AXValueType, kAXTrustedCheckOptionPrompt,
};
use objc2_core_foundation::{
    CFArray, CFBoolean, CFDictionary, CFRetained, CFRunLoop, CFString, CFType, CGPoint, CGRect,
    CGSize, kCFRunLoopCommonModes,
};
use objc2_foundation::{
    NSNotification, NSNotificationCenter, NSObjectProtocol, NSOperationQueue, NSRunLoop,
    NSRunLoopCommonModes, NSTimer,
};
use serde::Deserialize;
use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap},
    ffi::c_void,
    panic::AssertUnwindSafe,
    ptr::NonNull,
    thread,
    time::{Duration, Instant},
};

const EMBEDDED_CONFIG: &str = include_str!("../config.toml");
type Config = BTreeMap<String, Rule>;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Rule {
    #[serde(skip)]
    bundle_id: String,
    width: f64,
    height: f64,
    x: Option<f64>,
    y: Option<f64>,
}

fn parse_config(source: &str) -> Result<Config, String> {
    // TOML treats [com.apple.finder] as nested tables. Flatten their paths into bundle IDs.
    fn collect(table: toml::Table, path: String, rules: &mut Config) -> Result<(), String> {
        if !table.is_empty() && table.values().all(toml::Value::is_table) {
            for (key, value) in table {
                let child = if path.is_empty() {
                    key
                } else {
                    format!("{path}.{key}")
                };
                let toml::Value::Table(table) = value else {
                    unreachable!()
                };
                collect(table, child, rules)?;
            }
            return Ok(());
        }
        if path.is_empty() || path.chars().any(char::is_whitespace) {
            return Err("expected [application.bundle.id] with width and height".into());
        }
        let mut rule: Rule = toml::Value::Table(table)
            .try_into()
            .map_err(|e| format!("{path}: {e}"))?;
        if !rule.width.is_finite()
            || rule.width <= 0.0
            || !rule.height.is_finite()
            || rule.height <= 0.0
        {
            return Err(format!(
                "{path}: width and height must be finite and positive"
            ));
        }
        if [rule.x, rule.y]
            .into_iter()
            .flatten()
            .any(|n| !n.is_finite())
        {
            return Err(format!("{path}: x and y must be finite"));
        }
        rule.bundle_id = path.clone();
        if rules.insert(path.clone(), rule).is_some() {
            return Err(format!("duplicate application: {path}"));
        }
        Ok(())
    }
    let table = toml::from_str(source).map_err(|e| e.to_string())?;
    let mut rules = Config::new();
    collect(table, String::new(), &mut rules)?;
    Ok(rules)
}

fn main() {
    if let Err(error) = parse_config(EMBEDDED_CONFIG).and_then(run) {
        eprintln!("MacWindowResizer: {error}");
        std::process::exit(1);
    }
}

thread_local! {
    static MANAGER: RefCell<Option<Manager>> = const { RefCell::new(None) };
}

fn run(config: Config) -> Result<(), String> {
    let main_thread = MainThreadMarker::new().ok_or("AppKit requires the main thread")?;
    let workspace = NSWorkspace::sharedWorkspace();
    let application = NSApplication::sharedApplication(main_thread);
    application.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    let manager = Manager::new(config, workspace);
    MANAGER.with(|state| *state.borrow_mut() = Some(manager));
    MANAGER.with(|state| {
        let mut state = state.borrow_mut();
        let manager = state.as_mut().expect("manager initialized");
        manager.check_permission();
        for app in manager.workspace.runningApplications() {
            manager.request_attach(&app, false);
        }
    });
    application.run();
    MANAGER.with(|state| *state.borrow_mut() = None);
    Ok(())
}

struct Manager {
    config: Config,
    workspace: Retained<NSWorkspace>,
    center: Retained<NSNotificationCenter>,
    tokens: Vec<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
    observations: HashMap<i32, Observation>,
    pending: HashMap<i32, PendingAttachment>,
    retry_timer: Option<Retained<NSTimer>>,
}

struct PendingAttachment {
    app: Retained<NSRunningApplication>,
    recovery: StartupRecovery,
}

struct StartupRecovery {
    launched: bool,
    deadline: Instant,
}

impl StartupRecovery {
    fn new(launched: bool, now: Instant) -> Self {
        Self {
            launched,
            deadline: now + Duration::from_secs(5),
        }
    }

    fn active(&self, now: Instant) -> bool {
        now < self.deadline
    }
}

impl Manager {
    fn new(config: Config, workspace: Retained<NSWorkspace>) -> Self {
        let center = workspace.notificationCenter();
        let queue = NSOperationQueue::mainQueue();
        let mut tokens = Vec::new();
        // SAFETY: These constants are immutable Foundation notification names.
        let notifications = unsafe {
            [
                NSWorkspaceDidLaunchApplicationNotification,
                NSWorkspaceDidActivateApplicationNotification,
                NSWorkspaceDidTerminateApplicationNotification,
            ]
        };
        for (index, name) in notifications.into_iter().enumerate() {
            let block = RcBlock::new(move |notification: NonNull<NSNotification>| {
                // Catch Rust panics before returning through Objective-C.
                let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
                    // SAFETY: NSNotificationCenter supplies a live notification for this call.
                    let notification = unsafe { notification.as_ref() };
                    let Some(info) = notification.userInfo() else {
                        return;
                    };
                    // SAFETY: The workspace key is an immutable system constant.
                    let Some(object) = info.objectForKey(unsafe { NSWorkspaceApplicationKey })
                    else {
                        return;
                    };
                    let Some(app) = object.downcast_ref::<NSRunningApplication>() else {
                        return;
                    };
                    MANAGER.with(|state| {
                        let Ok(mut state) = state.try_borrow_mut() else {
                            return;
                        };
                        let Some(manager) = state.as_mut() else {
                            return;
                        };
                        if index == 2 {
                            manager.observations.remove(&app.processIdentifier());
                            manager.pending.remove(&app.processIdentifier());
                        } else {
                            manager.request_attach(app, index == 0);
                        }
                    });
                }));
                if result.is_err() {
                    eprintln!("MacWindowResizer: workspace callback panicked");
                }
            });
            // SAFETY: No Objective-C object filter; the block captures only an integer.
            // Delivery is explicitly on the main queue, which owns MANAGER.
            let token = unsafe {
                center.addObserverForName_object_queue_usingBlock(
                    Some(name),
                    None,
                    Some(&queue),
                    &block,
                )
            };
            tokens.push(token);
        }
        Self {
            config,
            workspace,
            center,
            tokens,
            observations: HashMap::new(),
            pending: HashMap::new(),
            retry_timer: None,
        }
    }

    fn check_permission(&self) {
        let prompt = CFBoolean::new(true);
        // SAFETY: The system key is a CFString and this option requires a CFBoolean.
        let options =
            CFDictionary::from_slices(&[unsafe { kAXTrustedCheckOptionPrompt }], &[prompt]);
        // SAFETY: The dictionary's key/value types satisfy the API's option schema.
        if !unsafe { AXIsProcessTrustedWithOptions(Some(options.as_opaque())) } {
            eprintln!(
                "MacWindowResizer: waiting for Accessibility permission. Enable the installed app in System Settings > Privacy & Security > Accessibility, then activate a configured application."
            );
        }
    }

    fn request_attach(&mut self, app: &NSRunningApplication, launched: bool) {
        let Some(bundle_id) = app.bundleIdentifier().map(|id| id.to_string()) else {
            return;
        };
        if !self.config.contains_key(&bundle_id) || app.isTerminated() {
            return;
        }
        let pid = app.processIdentifier();
        if self.observations.contains_key(&pid) && !launched {
            return;
        }
        // Activation must neither erase launch recovery nor extend its deadline.
        self.pending
            .entry(pid)
            .and_modify(|pending| {
                pending.recovery.launched |= launched;
            })
            .or_insert_with(|| PendingAttachment {
                app: app.retain(),
                recovery: StartupRecovery::new(launched, Instant::now()),
            });
        self.retry_pending();
        if self.pending.is_empty() || self.retry_timer.is_some() {
            return;
        }
        let block = RcBlock::new(|_: NonNull<NSTimer>| {
            let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
                MANAGER.with(|state| {
                    if let Ok(mut state) = state.try_borrow_mut()
                        && let Some(manager) = state.as_mut()
                    {
                        manager.retry_pending();
                    }
                });
            }));
            if result.is_err() {
                eprintln!("MacWindowResizer: attachment retry callback panicked");
            }
        });
        // SAFETY: The block captures nothing and is scheduled exclusively on the main
        // run loop, which owns MANAGER. Drop invalidates the timer before teardown.
        let timer = unsafe { NSTimer::timerWithTimeInterval_repeats_block(0.2, true, &block) };
        unsafe { NSRunLoop::mainRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes) };
        self.retry_timer = Some(timer);
    }

    fn retry_pending(&mut self) {
        let pids: Vec<_> = self.pending.keys().copied().collect();
        for pid in pids {
            let pending = &self.pending[&pid];
            if pending.app.isTerminated() || !pending.recovery.active(Instant::now()) {
                if !pending.app.isTerminated() {
                    eprintln!("MacWindowResizer: startup retry expired for pid {pid}");
                }
                self.pending.remove(&pid);
                continue;
            }
            // SAFETY: Process-wide permission query without prompting.
            if !unsafe { AXIsProcessTrusted() } {
                continue;
            }
            if !self.observations.contains_key(&pid) {
                let Some(bundle_id) = pending.app.bundleIdentifier().map(|id| id.to_string())
                else {
                    self.pending.remove(&pid);
                    continue;
                };
                match Observation::new(pid, self.config[&bundle_id].clone()) {
                    Ok(observation) => {
                        self.observations.insert(pid, observation);
                        eprintln!("MacWindowResizer: listening to {bundle_id} (pid {pid})");
                    }
                    Err(error) => {
                        eprintln!(
                            "MacWindowResizer: {bundle_id}: {error}; retrying during startup"
                        );
                        continue;
                    }
                }
            }
            if !pending.recovery.launched || self.observations[&pid].recover_startup_windows() {
                self.pending.remove(&pid);
            }
        }
        if self.pending.is_empty()
            && let Some(timer) = self.retry_timer.take()
        {
            timer.invalidate();
        }
    }
}

impl Drop for Manager {
    fn drop(&mut self) {
        if let Some(timer) = self.retry_timer.take() {
            timer.invalidate();
        }
        for token in &self.tokens {
            // SAFETY: Each token was returned by this notification center.
            unsafe { self.center.removeObserver((**token).as_ref()) };
        }
    }
}

struct Observation {
    observer: CFRetained<AXObserver>,
    application: CFRetained<AXUIElement>,
    // Heap allocation keeps the callback address stable even when this struct moves.
    _context: Box<WindowContext>,
    run_loop: CFRetained<CFRunLoop>,
}

impl Observation {
    fn new(pid: i32, rule: Rule) -> Result<Self, String> {
        let run_loop = CFRunLoop::main().ok_or("main run loop unavailable")?;
        // SAFETY: A running application's pid is valid input; the API returns an owned AX object.
        let application = unsafe { AXUIElement::new_application(pid) };
        let mut raw = std::ptr::null_mut();
        // SAFETY: raw is writable and the callback honors the API's lifetime/signature contract.
        let result =
            unsafe { AXObserver::create(pid, Some(window_event), NonNull::from(&mut raw)) };
        if result != AXError::Success {
            return Err(format!("AXObserverCreate: {}", result.0));
        }
        let raw = NonNull::new(raw).ok_or("AXObserverCreate returned a null observer")?;
        // SAFETY: AXObserverCreate follows the Create rule and gave us +1 ownership.
        let observer = unsafe { CFRetained::from_raw(raw) };
        let mut context = Box::new(WindowContext {
            rule,
            windows: RefCell::new(CreatedWindows {
                entries: Vec::new(),
            }),
        });
        // SAFETY: context lives until notification removal in Drop. All callbacks use this
        // main run loop, so context cannot be destroyed concurrently with a callback.
        let result = unsafe {
            observer.add_notification(
                &application,
                &CFString::from_str("AXWindowCreated"),
                std::ptr::from_mut(&mut *context).cast(),
            )
        };
        if result != AXError::Success {
            return Err(format!("AXObserverAddNotification: {}", result.0));
        }
        // A new window may become main only after AXWindowCreated is delivered.
        // SAFETY: The same context lifetime and main-run-loop guarantees apply.
        let result = unsafe {
            observer.add_notification(
                &application,
                &CFString::from_str("AXMainWindowChanged"),
                std::ptr::from_mut(&mut *context).cast(),
            )
        };
        if result != AXError::Success {
            eprintln!(
                "MacWindowResizer: {} main-window notifications unavailable ({}); only windows already main at creation can be adjusted",
                context.rule.bundle_id, result.0
            );
        }
        // SAFETY: The retained observer owns its source; the run loop retains an added source.
        let source = unsafe { observer.run_loop_source() };
        // SAFETY: The common-modes constant is immutable and valid for this process.
        run_loop.add_source(Some(&source), unsafe { kCFRunLoopCommonModes });
        Ok(Self {
            observer,
            application,
            _context: context,
            run_loop,
        })
    }

    fn recover_startup_windows(&self) -> bool {
        // Register first, then enumerate: a window created in between is deduplicated
        // against its queued AXWindowCreated event by the shared window registry.
        let Ok(value) = attribute(&self.application, "AXWindows") else {
            return false;
        };
        let Some(windows) = value.downcast_ref::<CFArray>() else {
            return false;
        };
        // SAFETY: AXWindows is an array of retained CF objects. Downcast each
        // element separately rather than assuming every object is an AXUIElement.
        let windows = unsafe { &*(windows as *const CFArray).cast::<CFArray<CFType>>() };
        let mut found = false;
        for value in windows {
            let Some(window) = value.downcast_ref::<AXUIElement>() else {
                continue;
            };
            if !is_standard_window(window) {
                continue;
            }
            self._context
                .handle(&self.observer, window, "AXWindowCreated");
            found |= self._context.is_main_window(window)
                && self
                    ._context
                    .windows
                    .borrow()
                    .entries
                    .iter()
                    .any(|(entry, _)| &**entry == window);
        }
        if found {
            eprintln!(
                "MacWindowResizer: recovered startup windows for {}",
                self._context.rule.bundle_id
            );
        }
        found
    }
}

impl Drop for Observation {
    fn drop(&mut self) {
        // SAFETY: All operations run on the main thread, before the context is released.
        unsafe {
            let _ = self
                .observer
                .remove_notification(&self.application, &CFString::from_str("AXWindowCreated"));
            let _ = self.observer.remove_notification(
                &self.application,
                &CFString::from_str("AXMainWindowChanged"),
            );
            for (window, _) in &self._context.windows.borrow().entries {
                let _ = self
                    .observer
                    .remove_notification(window, &CFString::from_str("AXUIElementDestroyed"));
            }
            self.run_loop.remove_source(
                Some(&self.observer.run_loop_source()),
                kCFRunLoopCommonModes,
            );
        }
    }
}

struct WindowContext {
    rule: Rule,
    windows: RefCell<CreatedWindows<CFRetained<AXUIElement>>>,
}

// Track creation events and windows recovered from an observed application launch.
// Keeping handled windows until destruction prevents duplicate creation events
// or later main-window changes from overwriting the user's manual resizing.
struct CreatedWindows<T> {
    entries: Vec<(T, bool)>,
}

impl<T: PartialEq> CreatedWindows<T> {
    fn contains(&self, window: &T) -> bool {
        self.entries.iter().any(|(entry, _)| entry == window)
    }

    fn remember(&mut self, window: T) {
        if !self.contains(&window) {
            self.entries.push((window, false));
        }
    }

    fn claim(&mut self, window: &T) -> bool {
        let Some((_, handled)) = self.entries.iter_mut().find(|(entry, _)| entry == window) else {
            return false;
        };
        !std::mem::replace(handled, true)
    }

    fn forget(&mut self, window: &T) {
        self.entries.retain(|(entry, _)| entry != window);
    }
}

impl WindowContext {
    fn handle(&self, observer: &AXObserver, window: &AXUIElement, notification: &str) {
        // SAFETY: The callback supplies a live local CF object, even for destruction
        // events. Retention and equality remain valid after the remote window closes.
        let window = unsafe { CFRetained::retain(NonNull::from(window)) };
        match notification {
            "AXUIElementDestroyed" => {
                self.windows.borrow_mut().forget(&window);
                return;
            }
            "AXWindowCreated" if !self.windows.borrow().contains(&window) => {
                // SAFETY: Observation owns self until all notifications are removed.
                let result = unsafe {
                    observer.add_notification(
                        &window,
                        &CFString::from_str("AXUIElementDestroyed"),
                        std::ptr::from_ref(self).cast_mut().cast(),
                    )
                };
                if result != AXError::Success && result != AXError::NotificationAlreadyRegistered {
                    eprintln!(
                        "MacWindowResizer: {} cannot track window lifetime ({}); skipped",
                        self.rule.bundle_id, result.0
                    );
                    return;
                }
                self.windows.borrow_mut().remember(window.clone());
            }
            "AXWindowCreated" | "AXMainWindowChanged" => {}
            _ => return,
        }
        // Never claim existing windows or windows with an unknown main status.
        // No RefCell borrow is held across accessibility calls.
        let known = self.windows.borrow().contains(&window);
        if !known || !self.is_main_window(&window) {
            return;
        }
        let claimed = self.windows.borrow_mut().claim(&window);
        if claimed {
            self.apply(&window);
        }
    }

    fn is_main_window(&self, window: &AXUIElement) -> bool {
        boolean_attribute(window, "AXMain") && is_standard_window(window)
    }

    fn apply(&self, window: &AXUIElement) {
        if !self.is_main_window(window) {
            return;
        }
        if boolean_attribute(window, "AXFullScreen") || boolean_attribute(window, "AXMinimized") {
            return;
        }
        // Move only as far as necessary before growing a window, otherwise AppKit
        // can report success while clamping AXSize to the remaining screen space.
        let position = point_attribute(window, "AXPosition")
            .map(|position| resize_position(position, &self.rule));
        let mut size = CGSize::new(self.rule.width, self.rule.height);
        // SAFETY: size points to a correctly aligned CGSize for AXValueType::CGSize.
        let Some(size) =
            (unsafe { AXValue::new(AXValueType::CGSize, NonNull::from(&mut size).cast()) })
        else {
            return;
        };
        for attempt in 0..3 {
            if attempt > 0 {
                thread::sleep(Duration::from_millis(100));
            }
            // The user may open a dialog or switch windows during a retry.
            if !self.is_main_window(window)
                || boolean_attribute(window, "AXFullScreen")
                || boolean_attribute(window, "AXMinimized")
            {
                break;
            }
            if let Some(position) = position {
                set_position(window, position, &self.rule.bundle_id);
            }
            // SAFETY: AXSize accepts a CGSize wrapped in an AXValue; both references are live.
            let result =
                unsafe { window.set_attribute_value(&CFString::from_str("AXSize"), &size) };
            if result != AXError::Success {
                eprintln!(
                    "MacWindowResizer: {} resize failed: {}",
                    self.rule.bundle_id, result.0
                );
                continue;
            }
            if let Some(position) = position {
                set_position(window, position, &self.rule.bundle_id);
            }
        }
        match size_attribute(window) {
            Some(actual) if actual == CGSize::new(self.rule.width, self.rule.height) => {
                eprintln!(
                    "MacWindowResizer: applied {} × {} to {} window",
                    actual.width, actual.height, self.rule.bundle_id
                );
            }
            Some(actual) => eprintln!(
                "MacWindowResizer: {} requested {} × {}, actual {} × {}",
                self.rule.bundle_id, self.rule.width, self.rule.height, actual.width, actual.height
            ),
            None => eprintln!(
                "MacWindowResizer: {} could not verify window size",
                self.rule.bundle_id
            ),
        }
    }
}

unsafe extern "C-unwind" fn window_event(
    observer: NonNull<AXObserver>,
    window: NonNull<AXUIElement>,
    notification: NonNull<CFString>,
    context: *mut c_void,
) {
    if context.is_null() {
        return;
    }
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: Observation owns this context and removes its notification before
        // dropping it. AX guarantees window is valid for the duration of this callback.
        unsafe {
            (&*context.cast::<WindowContext>()).handle(
                observer.as_ref(),
                window.as_ref(),
                &notification.as_ref().to_string(),
            )
        };
    }));
    if result.is_err() {
        eprintln!("MacWindowResizer: window callback panicked");
    }
}

fn attribute(element: &AXUIElement, name: &str) -> Result<CFRetained<CFType>, AXError> {
    let mut raw = std::ptr::null();
    // SAFETY: raw is writable; AX copies out a retained CF object on success.
    let result =
        unsafe { element.copy_attribute_value(&CFString::from_str(name), NonNull::from(&mut raw)) };
    if result != AXError::Success {
        return Err(result);
    }
    let raw = NonNull::new(raw.cast_mut()).ok_or(AXError::NoValue)?;
    // SAFETY: The Copy rule transfers +1 ownership of this non-null CFType.
    Ok(unsafe { CFRetained::from_raw(raw) })
}

fn is_standard_window(window: &AXUIElement) -> bool {
    attribute(window, "AXSubrole")
        .ok()
        .and_then(|value| {
            value
                .downcast_ref::<CFString>()
                .map(|role| role.to_string())
        })
        .is_some_and(|role| role == "AXStandardWindow")
}

fn boolean_attribute(element: &AXUIElement, name: &str) -> bool {
    attribute(element, name)
        .ok()
        .and_then(|value| value.downcast_ref::<CFBoolean>().map(|b| b.value()))
        .unwrap_or(false)
}

fn point_attribute(element: &AXUIElement, name: &str) -> Option<CGPoint> {
    let value = attribute(element, name).ok()?;
    let value = value.downcast_ref::<AXValue>()?;
    let mut point = CGPoint::new(0.0, 0.0);
    // SAFETY: We request a CGPoint and provide writable storage of that exact type.
    unsafe { value.value(AXValueType::CGPoint, NonNull::from(&mut point).cast()) }.then_some(point)
}

fn size_attribute(window: &AXUIElement) -> Option<CGSize> {
    let value = attribute(window, "AXSize").ok()?;
    let value = value.downcast_ref::<AXValue>()?;
    let mut size = CGSize::new(0.0, 0.0);
    // SAFETY: Writable CGSize storage matches the requested AXValue type.
    unsafe { value.value(AXValueType::CGSize, NonNull::from(&mut size).cast()) }.then_some(size)
}

fn set_position(window: &AXUIElement, mut position: CGPoint, bundle_id: &str) {
    if point_attribute(window, "AXPosition") == Some(position) {
        return;
    }
    // SAFETY: AXPosition takes a CGPoint wrapped in an AXValue.
    if let Some(value) =
        unsafe { AXValue::new(AXValueType::CGPoint, NonNull::from(&mut position).cast()) }
    {
        let result =
            unsafe { window.set_attribute_value(&CFString::from_str("AXPosition"), &value) };
        if result != AXError::Success {
            eprintln!(
                "MacWindowResizer: {bundle_id} position change failed: {}",
                result.0
            );
        }
    }
}

fn fit_position(position: CGPoint, size: CGSize, visible: CGRect) -> CGPoint {
    // Leave one logical point for the window border at the screen/Dock edges.
    let left = visible.origin.x + 1.0;
    let top = visible.origin.y + 1.0;
    CGPoint::new(
        position.x.clamp(
            left,
            (visible.origin.x + visible.size.width - size.width - 1.0).max(left),
        ),
        position.y.clamp(
            top,
            (visible.origin.y + visible.size.height - size.height - 1.0).max(top),
        ),
    )
}

fn resize_position(old: CGPoint, rule: &Rule) -> CGPoint {
    let desired = CGPoint::new(rule.x.unwrap_or(old.x), rule.y.unwrap_or(old.y));
    let Some(main_thread) = MainThreadMarker::new() else {
        return desired;
    };
    let screens = NSScreen::screens(main_thread);
    let Some(primary) = screens.firstObject() else {
        return desired;
    };
    // NSScreen uses bottom-left coordinates; AX uses the primary screen's top-left.
    let primary_top = primary.frame().origin.y + primary.frame().size.height;
    for screen in &screens {
        let frame = screen.frame();
        let top = primary_top - frame.origin.y - frame.size.height;
        if desired.x < frame.origin.x
            || desired.x >= frame.origin.x + frame.size.width
            || desired.y < top
            || desired.y >= top + frame.size.height
        {
            continue;
        }
        let visible = screen.visibleFrame();
        let visible = CGRect::new(
            CGPoint::new(
                visible.origin.x,
                primary_top - visible.origin.y - visible.size.height,
            ),
            visible.size,
        );
        let fitted = fit_position(desired, CGSize::new(rule.width, rule.height), visible);
        // Explicit coordinates remain authoritative, including multi-screen layouts.
        return CGPoint::new(rule.x.unwrap_or(fitted.x), rule.y.unwrap_or(fitted.y));
    }
    desired
}

#[cfg(test)]
mod tests {
    use super::*;
    const FINDER: &str = "[com.apple.finder]\nwidth = 860\nheight = 660\n";

    #[test]
    fn dotted_application_tables_and_optional_positions() {
        let config = parse_config(&format!(
            "{FINDER}\n[com.apple.Safari]\nwidth = 1200.5\nheight = 800\nx = -1400\ny = 41"
        ))
        .unwrap();
        let finder = &config["com.apple.finder"];
        assert_eq!(
            (finder.width, finder.height, finder.x, finder.y),
            (860.0, 660.0, None, None)
        );
        let safari = &config["com.apple.Safari"];
        assert_eq!(
            (safari.width, safari.x, safari.y),
            (1200.5, Some(-1400.0), Some(41.0))
        );
    }

    #[test]
    fn invalid_and_conflicting_rules_are_rejected() {
        for size in ["0", "-1", "nan", "inf"] {
            assert!(parse_config(&FINDER.replace("860", size)).is_err());
        }
        for extra in ["widht = 10", "x = nan", "y = inf"] {
            assert!(parse_config(&format!("{FINDER}{extra}")).is_err());
        }
        assert!(parse_config("").is_err());
        assert!(parse_config(&FINDER.replace("height = 660", "")).is_err());
        assert!(
            parse_config(&format!(
                "{FINDER}\n[\"com.apple.finder\"]\nwidth = 900\nheight = 700"
            ))
            .is_err()
        );
    }

    #[test]
    fn embedded_config_and_quoted_bundle_ids_are_supported() {
        parse_config(EMBEDDED_CONFIG).unwrap();
        assert!(
            parse_config(&FINDER.replace("[com.apple.finder]", "[\"com.apple.finder\"]"))
                .unwrap()
                .contains_key("com.apple.finder")
        );
    }

    #[test]
    fn growing_windows_move_only_when_the_target_would_cross_screen_edges() {
        let visible = CGRect::new(CGPoint::new(0.0, 33.0), CGSize::new(1728.0, 1026.0));
        let target = CGSize::new(1400.0, 985.0);
        assert_eq!(
            fit_position(CGPoint::new(357.0, 170.0), target, visible),
            CGPoint::new(327.0, 73.0)
        );
        let fitting = CGPoint::new(100.0, 50.0);
        assert_eq!(fit_position(fitting, target, visible), fitting);
        let secondary = CGRect::new(CGPoint::new(-1920.0, -200.0), CGSize::new(1920.0, 1080.0));
        assert_eq!(
            fit_position(CGPoint::new(-500.0, 300.0), target, secondary),
            CGPoint::new(-1401.0, -106.0)
        );
        // An oversized request must not panic from an inverted clamp interval.
        assert_eq!(
            fit_position(fitting, CGSize::new(3000.0, 2000.0), visible),
            CGPoint::new(1.0, 34.0)
        );
    }

    #[test]
    fn startup_recovery_is_bounded_and_does_not_include_preexisting_apps() {
        let now = Instant::now();
        let launch = StartupRecovery::new(true, now);
        assert!(launch.launched && launch.active(now + Duration::from_millis(200)));
        assert!(!launch.active(now + Duration::from_secs(5)));
        let existing = StartupRecovery::new(false, now);
        assert!(existing.active(now));
        assert!(!existing.launched);
    }

    #[test]
    fn startup_snapshot_and_creation_event_share_one_resize_claim() {
        let mut windows = CreatedWindows {
            entries: Vec::new(),
        };
        // Recovery sees a window before it becomes main; creation arrives later.
        windows.remember(1);
        windows.remember(1);
        assert_eq!(windows.entries.len(), 1);
        assert!(windows.claim(&1));
        // A later snapshot or notification must preserve a user's manual resize.
        windows.remember(1);
        assert!(!windows.claim(&1));
        // This must work in the opposite event order too.
        windows.remember(2);
        assert!(windows.claim(&2));
        windows.remember(2);
        assert!(!windows.claim(&2));
    }

    #[test]
    fn newly_created_windows_are_adjusted_only_once_when_they_become_main() {
        let mut windows = CreatedWindows {
            entries: Vec::new(),
        };
        // An already-open window becoming main must not be resized.
        assert!(!windows.claim(&1));
        // Creation may precede becoming main; retain the pending window.
        windows.entries.push((2, false));
        assert!(windows.contains(&2));
        assert!(windows.claim(&2));
        // Returning to this window or receiving a duplicate creation changes nothing.
        assert!(windows.contains(&2));
        assert!(!windows.claim(&2));
    }

    #[test]
    fn destroyed_windows_are_forgotten_without_affecting_other_windows() {
        let mut windows = CreatedWindows {
            entries: vec![(1, false), (2, true), (3, false)],
        };
        windows.forget(&1);
        windows.forget(&2);
        assert!(!windows.claim(&1));
        assert!(!windows.contains(&2));
        assert!(windows.claim(&3));
        // A closed window's identifier may later be reused by a new window.
        windows.entries.push((2, false));
        assert!(windows.claim(&2));
    }
}
