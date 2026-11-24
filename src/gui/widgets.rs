use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSAlertStyle, NSAutoresizingMaskOptions, NSBezelStyle,
    NSBox, NSBoxType, NSButton, NSButtonType, NSColor, NSControl, NSControlStateValueOff,
    NSControlStateValueOn, NSFont, NSFontWeightMedium, NSFontWeightSemibold, NSModalResponseOK,
    NSOpenPanel, NSPasteboard, NSPasteboardTypeString, NSPopUpButton, NSProgressIndicator,
    NSProgressIndicatorStyle, NSSlider, NSTextField, NSTitlePosition, NSView,
};
use objc2_core_foundation::{CGFloat, CGPoint, CGRect, CGSize};
use objc2_foundation::{NSObject, NSObjectProtocol, NSRect, NSSize, NSString};
use std::cell::RefCell;
use std::path::PathBuf;

/// A simple `NSView` subclass using top-left coordinates (`isFlipped = true`).
pub struct FlippedViewIvars;

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[ivars = FlippedViewIvars]
    pub struct FlippedView;

    unsafe impl NSObjectProtocol for FlippedView {}

    impl FlippedView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }
    }
);

impl FlippedView {
    pub fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(FlippedViewIvars);
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }
}

/// Reusable Objective-C target that forwards control/menu actions into a Rust closure.
pub struct ClosureTargetIvars {
    callback: RefCell<Box<dyn FnMut(&AnyObject)>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = ClosureTargetIvars]
    pub struct ClosureTarget;

    unsafe impl NSObjectProtocol for ClosureTarget {}

    impl ClosureTarget {
        #[unsafe(method(invoke:))]
        fn invoke(&self, sender: &AnyObject) {
            if let Ok(mut cb) = self.ivars().callback.try_borrow_mut() {
                (cb)(sender);
            }
        }
    }
);

impl ClosureTarget {
    pub fn new<F>(mtm: MainThreadMarker, f: F) -> Retained<Self>
    where
        F: FnMut(&AnyObject) + 'static,
    {
        let this = mtm.alloc::<Self>().set_ivars(ClosureTargetIvars {
            callback: RefCell::new(Box::new(f)),
        });
        unsafe { msg_send![super(this), init] }
    }
}

/// Keeps `ClosureTarget` instances alive for the lifetime of a view or window.
#[derive(Default)]
pub struct TargetStore {
    targets: Vec<Retained<ClosureTarget>>,
}

impl TargetStore {
    pub fn new() -> Self {
        Self {
            targets: Vec::new(),
        }
    }

    pub fn clear(&mut self) {
        self.targets.clear();
    }

    pub fn bind<F>(&mut self, mtm: MainThreadMarker, control: &NSControl, f: F)
    where
        F: FnMut(&AnyObject) + 'static,
    {
        let target = ClosureTarget::new(mtm, f);
        unsafe {
            control.setTarget(Some(&target));
            control.setAction(Some(sel!(invoke:)));
        }
        self.targets.push(target);
    }

    pub fn bind_click<F>(&mut self, mtm: MainThreadMarker, control: &NSControl, mut f: F)
    where
        F: FnMut() + 'static,
    {
        self.bind(mtm, control, move |_| f());
    }

    pub fn add_target(&mut self, target: Retained<ClosureTarget>) {
        self.targets.push(target);
    }
}

pub fn rect(x: f64, y: f64, w: f64, h: f64) -> NSRect {
    CGRect::new(CGPoint::new(x, y), CGSize::new(w, h))
}

pub fn size(w: f64, h: f64) -> NSSize {
    CGSize::new(w, h)
}

pub fn label(mtm: MainThreadMarker, frame: NSRect, text: &str, font_size: f64) -> Retained<NSTextField> {
    let field = NSTextField::initWithFrame(mtm.alloc(), frame);
    field.setStringValue(&NSString::from_str(text));
    field.setEditable(false);
    field.setSelectable(false);
    field.setBezeled(false);
    field.setBordered(false);
    field.setDrawsBackground(false);
    field.setFont(Some(&NSFont::systemFontOfSize(font_size)));
    field.setTextColor(Some(&NSColor::labelColor()));
    field
}

pub fn bold_label(mtm: MainThreadMarker, frame: NSRect, text: &str, font_size: f64) -> Retained<NSTextField> {
    let field = label(mtm, frame, text, font_size);
    field.setFont(Some(&NSFont::boldSystemFontOfSize(font_size)));
    field
}

pub fn semibold_label(mtm: MainThreadMarker, frame: NSRect, text: &str, font_size: f64) -> Retained<NSTextField> {
    let field = label(mtm, frame, text, font_size);
    field.setFont(Some(&NSFont::systemFontOfSize_weight(font_size, unsafe { NSFontWeightSemibold })));
    field
}

pub fn secondary_label(mtm: MainThreadMarker, frame: NSRect, text: &str, font_size: f64) -> Retained<NSTextField> {
    let field = label(mtm, frame, text, font_size);
    field.setTextColor(Some(&NSColor::secondaryLabelColor()));
    field
}

pub fn selectable_mono_label(mtm: MainThreadMarker, frame: NSRect, text: &str, font_size: f64) -> Retained<NSTextField> {
    let field = label(mtm, frame, text, font_size);
    field.setSelectable(true);
    field.setFont(Some(&NSFont::monospacedSystemFontOfSize_weight(
        font_size,
        unsafe { NSFontWeightMedium },
    )));
    field.setTextColor(Some(&NSColor::secondaryLabelColor()));
    field
}

pub fn text_input(mtm: MainThreadMarker, frame: NSRect, value: &str, placeholder: &str) -> Retained<NSTextField> {
    let field = NSTextField::initWithFrame(mtm.alloc(), frame);
    field.setStringValue(&NSString::from_str(value));
    field.setPlaceholderString(Some(&NSString::from_str(placeholder)));
    field.setEditable(true);
    field.setSelectable(true);
    field.setBezeled(true);
    field.setDrawsBackground(true);
    field.setFont(Some(&NSFont::systemFontOfSize(13.0)));
    field
}

pub fn push_button<F>(
    mtm: MainThreadMarker,
    frame: NSRect,
    title: &str,
    targets: &mut TargetStore,
    on_click: F,
) -> Retained<NSButton>
where
    F: FnMut() + 'static,
{
    let btn = NSButton::initWithFrame(mtm.alloc(), frame);
    btn.setTitle(&NSString::from_str(title));
    btn.setBezelStyle(NSBezelStyle::Push);
    btn.setButtonType(NSButtonType::MomentaryPushIn);
    btn.setFont(Some(&NSFont::systemFontOfSize(13.0)));
    targets.bind_click(mtm, &btn, on_click);
    btn
}

pub fn primary_button<F>(
    mtm: MainThreadMarker,
    frame: NSRect,
    title: &str,
    targets: &mut TargetStore,
    on_click: F,
) -> Retained<NSButton>
where
    F: FnMut() + 'static,
{
    let btn = push_button(mtm, frame, title, targets, on_click);
    btn.setKeyEquivalent(&NSString::from_str("\r"));
    btn
}

pub fn checkbox<F>(
    mtm: MainThreadMarker,
    frame: NSRect,
    title: &str,
    checked: bool,
    targets: &mut TargetStore,
    on_toggle: F,
) -> Retained<NSButton>
where
    F: FnMut(bool) + 'static,
{
    let btn = NSButton::initWithFrame(mtm.alloc(), frame);
    btn.setTitle(&NSString::from_str(title));
    btn.setButtonType(NSButtonType::Switch);
    btn.setState(if checked {
        NSControlStateValueOn
    } else {
        NSControlStateValueOff
    });
    btn.setFont(Some(&NSFont::systemFontOfSize(13.0)));
    let mut on_toggle = on_toggle;
    targets.bind(mtm, &btn, move |sender| {
        let state: isize = unsafe { msg_send![sender, state] };
        on_toggle(state == NSControlStateValueOn);
    });
    btn
}

pub fn slider<F>(
    mtm: MainThreadMarker,
    frame: NSRect,
    min: f64,
    max: f64,
    value: f64,
    targets: &mut TargetStore,
    on_change: F,
) -> Retained<NSSlider>
where
    F: FnMut(f64) + 'static,
{
    let s = NSSlider::initWithFrame(mtm.alloc(), frame);
    s.setMinValue(min);
    s.setMaxValue(max);
    s.setDoubleValue(value.clamp(min, max));
    s.setContinuous(true);
    let mut on_change = on_change;
    targets.bind(mtm, &s, move |sender| {
        let val: f64 = unsafe { msg_send![sender, doubleValue] };
        on_change(val);
    });
    s
}

pub fn popup<F>(
    mtm: MainThreadMarker,
    frame: NSRect,
    items: &[&str],
    selected: usize,
    targets: &mut TargetStore,
    on_change: F,
) -> Retained<NSPopUpButton>
where
    F: FnMut(usize) + 'static,
{
    let p = NSPopUpButton::initWithFrame_pullsDown(mtm.alloc(), frame, false);
    for item in items {
        p.addItemWithTitle(&NSString::from_str(item));
    }
    if selected < items.len() {
        p.selectItemAtIndex(selected as isize);
    }
    let mut on_change = on_change;
    targets.bind(mtm, &p, move |sender| {
        let idx: isize = unsafe { msg_send![sender, indexOfSelectedItem] };
        if idx >= 0 {
            on_change(idx as usize);
        }
    });
    p
}

pub fn progress_bar(mtm: MainThreadMarker, frame: NSRect) -> Retained<NSProgressIndicator> {
    let p = NSProgressIndicator::initWithFrame(mtm.alloc(), frame);
    p.setStyle(NSProgressIndicatorStyle::Bar);
    p.setIndeterminate(false);
    p.setMinValue(0.0);
    p.setMaxValue(100.0);
    p.setDoubleValue(0.0);
    p
}

/// Creates a styled card container (`NSBox`) with a flipped content view.
pub fn card(mtm: MainThreadMarker, frame: NSRect) -> (Retained<NSBox>, Retained<FlippedView>) {
    let bx = NSBox::initWithFrame(mtm.alloc(), frame);
    bx.setBoxType(NSBoxType::Custom);
    bx.setTitlePosition(NSTitlePosition::NoTitle);
    bx.setCornerRadius(10.0 as CGFloat);
    bx.setBorderWidth(1.0 as CGFloat);
    bx.setBorderColor(&NSColor::separatorColor());
    bx.setFillColor(&NSColor::controlBackgroundColor());
    bx.setContentViewMargins(size(0.0, 0.0));
    let inner = FlippedView::new(mtm, rect(0.0, 0.0, frame.size.width, frame.size.height));
    inner.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
    );
    bx.setContentView(Some(&inner));
    (bx, inner)
}

pub fn tint_card(
    mtm: MainThreadMarker,
    frame: NSRect,
    fill: &NSColor,
    border: &NSColor,
    radius: f64,
) -> (Retained<NSBox>, Retained<FlippedView>) {
    let (bx, inner) = card(mtm, frame);
    bx.setFillColor(fill);
    bx.setBorderColor(border);
    bx.setCornerRadius(radius as CGFloat);
    (bx, inner)
}

pub fn separator(mtm: MainThreadMarker, frame: NSRect) -> Retained<NSBox> {
    let bx = NSBox::initWithFrame(mtm.alloc(), frame);
    bx.setBoxType(NSBoxType::Custom);
    bx.setTitlePosition(NSTitlePosition::NoTitle);
    bx.setBorderWidth(0.0);
    bx.setFillColor(&NSColor::separatorColor());
    bx
}

pub fn status_dot(mtm: MainThreadMarker, x: f64, y: f64, color: &NSColor) -> Retained<NSBox> {
    let bx = NSBox::initWithFrame(mtm.alloc(), rect(x, y, 10.0, 10.0));
    bx.setBoxType(NSBoxType::Custom);
    bx.setTitlePosition(NSTitlePosition::NoTitle);
    bx.setCornerRadius(5.0);
    bx.setBorderWidth(0.0);
    bx.setFillColor(color);
    bx
}

pub fn show_error_alert(mtm: MainThreadMarker, title: &str, message: &str) {
    let alert = NSAlert::new(mtm);
    alert.setAlertStyle(NSAlertStyle::Critical);
    alert.setMessageText(&NSString::from_str(title));
    alert.setInformativeText(&NSString::from_str(message));
    alert.addButtonWithTitle(&NSString::from_str("OK"));
    alert.runModal();
}

pub fn show_confirm_alert(
    mtm: MainThreadMarker,
    title: &str,
    message: &str,
    confirm_label: &str,
    destructive: bool,
) -> bool {
    let alert = NSAlert::new(mtm);
    alert.setAlertStyle(if destructive {
        NSAlertStyle::Critical
    } else {
        NSAlertStyle::Warning
    });
    alert.setMessageText(&NSString::from_str(title));
    alert.setInformativeText(&NSString::from_str(message));
    alert.addButtonWithTitle(&NSString::from_str(confirm_label));
    alert.addButtonWithTitle(&NSString::from_str("Cancel"));
    alert.runModal() == NSAlertFirstButtonReturn
}

pub fn pick_iso_file(mtm: MainThreadMarker) -> Option<PathBuf> {
    let panel = NSOpenPanel::openPanel(mtm);
    panel.setCanChooseFiles(true);
    panel.setCanChooseDirectories(false);
    panel.setAllowsMultipleSelection(false);
    panel.setTitle(Some(&NSString::from_str("Select ARM64 Linux ISO Image")));
    panel.setPrompt(Some(&NSString::from_str("Select ISO")));
    if panel.runModal() == NSModalResponseOK {
        let urls = panel.URLs();
        if !urls.is_empty() {
            let url = urls.objectAtIndex(0);
            if let Some(path) = url.path() {
                return Some(PathBuf::from(path.to_string()));
            }
        }
    }
    None
}

pub fn pick_directory(mtm: MainThreadMarker) -> Option<PathBuf> {
    let panel = NSOpenPanel::openPanel(mtm);
    panel.setCanChooseFiles(false);
    panel.setCanChooseDirectories(true);
    panel.setAllowsMultipleSelection(false);
    panel.setTitle(Some(&NSString::from_str("Choose Host Folder to Share with Linux")));
    panel.setPrompt(Some(&NSString::from_str("Share Folder")));
    if panel.runModal() == NSModalResponseOK {
        let urls = panel.URLs();
        if !urls.is_empty() {
            let url = urls.objectAtIndex(0);
            if let Some(path) = url.path() {
                return Some(PathBuf::from(path.to_string()));
            }
        }
    }
    None
}

pub fn copy_to_clipboard(text: &str) {
    let pb = NSPasteboard::generalPasteboard();
    pb.clearContents();
    unsafe {
        pb.setString_forType(&NSString::from_str(text), NSPasteboardTypeString);
    }
}
