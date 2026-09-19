//! Native window creation callbacks with config.toml embedded at compile time.

use block2::RcBlock;
use objc2::{MainThreadMarker, rc::Retained, runtime::ProtocolObject};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSRunningApplication, NSWorkspace,
    NSWorkspaceApplicationKey, NSWorkspaceDidActivateApplicationNotification,
    NSWorkspaceDidLaunchApplicationNotification, NSWorkspaceDidTerminateApplicationNotification,
};
use objc2_application_services::{
    AXError, AXIsProcessTrusted, AXIsProcessTrustedWithOptions, AXObserver, AXUIElement, AXValue,
    AXValueType, kAXTrustedCheckOptionPrompt,
};
use objc2_core_foundation::{
    CFBoolean, CFDictionary, CFRetained, CFRunLoop, CFString, CFType, CGPoint, CGSize,
    kCFRunLoopCommonModes,
};
use objc2_foundation::{NSNotification, NSNotificationCenter, NSObjectProtocol, NSOperationQueue};
use serde::Deserialize;
use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap},
    ffi::c_void,
    panic::AssertUnwindSafe,
    ptr::NonNull,
    thread,
    time::Duration,
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
            manager.attach(&app);
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
                        } else {
                            manager.attach(app);
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

    fn attach(&mut self, app: &NSRunningApplication) {
        let Some(bundle_id) = app.bundleIdentifier().map(|id| id.to_string()) else {
            return;
        };
        let Some(rule) = self.config.get(&bundle_id) else {
            return;
        };
        let pid = app.processIdentifier();
        // SAFETY: Process-wide permission query; no prompting or memory arguments.
        if app.isTerminated()
            || self.observations.contains_key(&pid)
            || !unsafe { AXIsProcessTrusted() }
        {
            return;
        }
        match Observation::new(pid, rule.clone()) {
            Ok(observation) => {
                self.observations.insert(pid, observation);
                eprintln!("MacWindowResizer: listening to {bundle_id} (pid {pid})");
            }
            Err(error) => eprintln!(
                "MacWindowResizer: {bundle_id}: {error}; will retry on application activation"
            ),
        }
    }
}

impl Drop for Manager {
    fn drop(&mut self) {
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

// Track only windows created while listening, including those already handled.
// Keeping handled windows until destruction prevents duplicate creation events
// or later main-window changes from overwriting the user's manual resizing.
struct CreatedWindows<T> {
    entries: Vec<(T, bool)>,
}

impl<T: PartialEq> CreatedWindows<T> {
    fn contains(&self, window: &T) -> bool {
        self.entries.iter().any(|(entry, _)| entry == window)
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
            "AXWindowCreated" => {
                if self.windows.borrow().contains(&window) {
                    return;
                }
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
                self.windows
                    .borrow_mut()
                    .entries
                    .push((window.clone(), false));
            }
            "AXMainWindowChanged" => {}
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
        if !boolean_attribute(window, "AXMain") {
            return false;
        }
        attribute(window, "AXSubrole")
            .ok()
            .and_then(|value| {
                value
                    .downcast_ref::<CFString>()
                    .map(|role| role.to_string())
            })
            .is_some_and(|role| role == "AXStandardWindow")
    }

    fn apply(&self, window: &AXUIElement) {
        if !self.is_main_window(window) {
            return;
        }
        if boolean_attribute(window, "AXFullScreen") || boolean_attribute(window, "AXMinimized") {
            return;
        }
        // Capture position before resizing: some applications move a window during resize.
        let old_position = point_attribute(window, "AXPosition");
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
            if let Some(mut position) = old_position {
                position.x = self.rule.x.unwrap_or(position.x);
                position.y = self.rule.y.unwrap_or(position.y);
                if point_attribute(window, "AXPosition") != Some(position) {
                    // SAFETY: position is a live CGPoint; AXPosition requires this value type.
                    if let Some(value) = unsafe {
                        AXValue::new(AXValueType::CGPoint, NonNull::from(&mut position).cast())
                    } {
                        let result = unsafe {
                            window.set_attribute_value(&CFString::from_str("AXPosition"), &value)
                        };
                        if result != AXError::Success {
                            eprintln!(
                                "MacWindowResizer: {} position change failed: {}",
                                self.rule.bundle_id, result.0
                            );
                        }
                    }
                }
            }
            eprintln!(
                "MacWindowResizer: applied {} × {} to {} window",
                self.rule.width, self.rule.height, self.rule.bundle_id
            );
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
