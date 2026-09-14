//! Finder document-open bridge for macOS `.feel` packages.

#[cfg(target_os = "macos")]
mod platform {
    use std::cell::RefCell;
    use std::path::PathBuf;
    use std::sync::{Mutex, OnceLock};

    use objc2::rc::Retained;
    use objc2::runtime::ProtocolObject;
    use objc2::{declare_class, msg_send_id, mutability, ClassType, DeclaredClass};
    use objc2_app_kit::{NSApplication, NSApplicationDelegate};
    use objc2_foundation::{MainThreadMarker, NSArray, NSObject, NSObjectProtocol, NSURL};

    static PENDING: OnceLock<Mutex<Vec<PathBuf>>> = OnceLock::new();

    thread_local! {
        static DELEGATE: RefCell<Option<Retained<RetroFeelAppDelegate>>> = const { RefCell::new(None) };
    }

    declare_class!(
        struct RetroFeelAppDelegate;

        unsafe impl ClassType for RetroFeelAppDelegate {
            type Super = NSObject;
            type Mutability = mutability::MainThreadOnly;
            const NAME: &'static str = "RetroFeelAppDelegate";
        }

        impl DeclaredClass for RetroFeelAppDelegate {
            type Ivars = Retained<ProtocolObject<dyn NSApplicationDelegate>>;
        }

        unsafe impl NSObjectProtocol for RetroFeelAppDelegate {}

        unsafe impl NSApplicationDelegate for RetroFeelAppDelegate {
            #[method(application:openURLs:)]
            fn application_open_urls(
                &self,
                _application: &NSApplication,
                urls: &NSArray<NSURL>,
            ) {
                let pending = PENDING.get_or_init(|| Mutex::new(Vec::new()));
                if let Ok(mut pending) = pending.lock() {
                    for url in urls {
                        if let Some(path) = unsafe { url.path() } {
                            pending.push(PathBuf::from(path.to_string()));
                        }
                    }
                }
            }

            #[method(applicationWillTerminate:)]
            fn application_will_terminate(
                &self,
                notification: &objc2_foundation::NSNotification,
            ) {
                // Winit installed the prior delegate while creating Bevy's
                // event loop. Preserve its termination bookkeeping.
                unsafe { self.ivars().applicationWillTerminate(notification) };
            }
        }
    );

    impl RetroFeelAppDelegate {
        fn new(
            mtm: MainThreadMarker,
            original: Retained<ProtocolObject<dyn NSApplicationDelegate>>,
        ) -> Retained<Self> {
            let this = mtm.alloc().set_ivars(original);
            unsafe { msg_send_id![super(this), init] }
        }
    }

    pub fn install() {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        DELEGATE.with(|slot| {
            if slot.borrow().is_some() {
                return;
            }
            let app = NSApplication::sharedApplication(mtm);
            let Some(original) = (unsafe { app.delegate() }) else {
                return;
            };
            let delegate = RetroFeelAppDelegate::new(mtm, original);
            app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
            *slot.borrow_mut() = Some(delegate);
        });
    }

    pub fn drain() -> Vec<PathBuf> {
        let Some(pending) = PENDING.get() else {
            return Vec::new();
        };
        let Ok(mut pending) = pending.lock() else {
            return Vec::new();
        };
        std::mem::take(&mut *pending)
    }
}

#[cfg(target_os = "macos")]
pub use platform::{drain, install};

#[cfg(not(target_os = "macos"))]
pub fn install() {}

#[cfg(not(target_os = "macos"))]
pub fn drain() -> Vec<std::path::PathBuf> {
    Vec::new()
}
