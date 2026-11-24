pub mod display;
pub mod downloader;
pub mod manager;
pub mod widgets;
pub mod wizard;

use anyhow::{Result, bail};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate, NSMenu, NSMenuItem,
};
use objc2_foundation::{NSNotification, NSObject, NSObjectProtocol, NSString};
use std::cell::RefCell;
use std::rc::Rc;

use crate::gui::manager::ManagerApp;
use crate::gui::widgets::{ClosureTarget, TargetStore};

struct AppDelegateIvars {
    manager: RefCell<Option<Rc<ManagerApp>>>,
    auto_start_vm_id: RefCell<Option<String>>,
    menu_targets: RefCell<TargetStore>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = AppDelegateIvars]
    struct RustVmAppDelegate;

    unsafe impl NSObjectProtocol for RustVmAppDelegate {}

    unsafe impl NSApplicationDelegate for RustVmAppDelegate {
        #[unsafe(method(applicationDidFinishLaunching:))]
        fn did_finish_launching(&self, _notification: &NSNotification) {
            let mtm = MainThreadMarker::from(self);
            let auto_id = self.ivars().auto_start_vm_id.borrow_mut().take();
            let mgr = ManagerApp::launch(mtm, auto_id);
            setup_menu_bar(mtm, &mgr, &mut self.ivars().menu_targets.borrow_mut());
            *self.ivars().manager.borrow_mut() = Some(mgr);

            let app = NSApplication::sharedApplication(mtm);
            #[allow(deprecated)]
            app.activateIgnoringOtherApps(true);
        }

        #[unsafe(method(applicationShouldTerminateAfterLastWindowClosed:))]
        fn should_terminate_after_last_window_closed(&self, _sender: &NSApplication) -> bool {
            true
        }

        #[unsafe(method(applicationWillTerminate:))]
        fn will_terminate(&self, _notification: &NSNotification) {
            if let Some(ref mgr) = *self.ivars().manager.borrow() {
                mgr.stop_all_sessions();
            }
        }
    }
);

impl RustVmAppDelegate {
    fn new(mtm: MainThreadMarker, auto_start_vm_id: Option<String>) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(AppDelegateIvars {
            manager: RefCell::new(None),
            auto_start_vm_id: RefCell::new(auto_start_vm_id),
            menu_targets: RefCell::new(TargetStore::new()),
        });
        unsafe { msg_send![super(this), init] }
    }
}

pub fn run_manager_app(auto_start_vm_id: Option<String>) -> Result<()> {
    let Some(mtm) = MainThreadMarker::new() else {
        bail!("RustVM GUI must be started on the main thread");
    };

    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);

    let delegate = RustVmAppDelegate::new(mtm, auto_start_vm_id);
    app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    app.run();
    Ok(())
}

fn setup_menu_bar(mtm: MainThreadMarker, mgr: &Rc<ManagerApp>, targets: &mut TargetStore) {
    let main_menu = NSMenu::new(mtm);

    // 1. Application Menu (RustVM)
    let app_item = NSMenuItem::new(mtm);
    main_menu.addItem(&app_item);
    let app_menu = NSMenu::initWithTitle(mtm.alloc(), &NSString::from_str("RustVM"));
    unsafe {
        let quit_item = app_menu.addItemWithTitle_action_keyEquivalent(
            &NSString::from_str("Quit RustVM"),
            Some(sel!(terminate:)),
            &NSString::from_str("q"),
        );
        let _ = quit_item;
    }
    app_item.setSubmenu(Some(&app_menu));

    // 2. File Menu (New VM, Get Linux OS, Close Window)
    let file_item = NSMenuItem::new(mtm);
    main_menu.addItem(&file_item);
    let file_menu = NSMenu::initWithTitle(mtm.alloc(), &NSString::from_str("File"));
    {
        let weak = Rc::downgrade(mgr);
        let t = ClosureTarget::new(mtm, move |_| {
            if let Some(m) = weak.upgrade() {
                m.open_create_wizard(None, None);
            }
        });
        let item = unsafe {
            file_menu.addItemWithTitle_action_keyEquivalent(
                &NSString::from_str("New Virtual Machine…"),
                Some(sel!(invoke:)),
                &NSString::from_str("n"),
            )
        };
        unsafe {
            item.setTarget(Some(&t));
        }
        targets.add_target(t);
    }
    {
        let weak = Rc::downgrade(mgr);
        let t = ClosureTarget::new(mtm, move |_| {
            if let Some(m) = weak.upgrade() {
                m.open_downloader(None);
            }
        });
        let item = unsafe {
            file_menu.addItemWithTitle_action_keyEquivalent(
                &NSString::from_str("Get Linux OS…"),
                Some(sel!(invoke:)),
                &NSString::from_str("d"),
            )
        };
        unsafe {
            item.setTarget(Some(&t));
        }
        targets.add_target(t);
    }
    file_menu.addItem(&NSMenuItem::separatorItem(mtm));
    {
        let weak = Rc::downgrade(mgr);
        let t = ClosureTarget::new(mtm, move |_| {
            if let Some(m) = weak.upgrade() {
                m.focus();
            }
        });
        let item = unsafe {
            file_menu.addItemWithTitle_action_keyEquivalent(
                &NSString::from_str("Show VM Manager"),
                Some(sel!(invoke:)),
                &NSString::from_str("m"),
            )
        };
        unsafe {
            item.setTarget(Some(&t));
        }
        targets.add_target(t);
    }
    unsafe {
        file_menu.addItemWithTitle_action_keyEquivalent(
            &NSString::from_str("Close Window"),
            Some(sel!(performClose:)),
            &NSString::from_str("w"),
        );
    }
    file_item.setSubmenu(Some(&file_menu));

    // 3. Edit Menu (enables Cmd+C/V/X/A in text inputs!)
    let edit_item = NSMenuItem::new(mtm);
    main_menu.addItem(&edit_item);
    let edit_menu = NSMenu::initWithTitle(mtm.alloc(), &NSString::from_str("Edit"));
    unsafe {
        edit_menu.addItemWithTitle_action_keyEquivalent(
            &NSString::from_str("Cut"),
            Some(sel!(cut:)),
            &NSString::from_str("x"),
        );
        edit_menu.addItemWithTitle_action_keyEquivalent(
            &NSString::from_str("Copy"),
            Some(sel!(copy:)),
            &NSString::from_str("c"),
        );
        edit_menu.addItemWithTitle_action_keyEquivalent(
            &NSString::from_str("Paste"),
            Some(sel!(paste:)),
            &NSString::from_str("v"),
        );
        edit_menu.addItemWithTitle_action_keyEquivalent(
            &NSString::from_str("Select All"),
            Some(sel!(selectAll:)),
            &NSString::from_str("a"),
        );
    }
    edit_item.setSubmenu(Some(&edit_menu));

    let app = NSApplication::sharedApplication(mtm);
    app.setMainMenu(Some(&main_menu));
}
