//! `aibo-axtarget` — the controlled AX test target (§18 tier 3).
//!
//! > `testapps/` is the important idea: a controlled AX/UIA target makes the
//! > platform layer genuinely testable instead of "run it and see". Build it in
//! > P0 alongside S2 — the spike and the test harness are the same work. — §18
//!
//! One window, five text controls, deterministic seed text with the exact
//! counts printed to stdout at launch. Deliberately plain AppKit: `NSTextField`,
//! `NSSecureTextField` and `NSTextView` are the controls whose AX behaviour is
//! *documented*, so a failure against this app is a bug in aibo, never an
//! ambiguity in the target.
//!
//! It requests no permissions and holds no state. Run it, put the caret in a
//! field, and point S2/S4/S7 at it.

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!(
        "aibo-axtarget is the macOS AX target. The Windows UIA target is a\n\
         separate app — §18 tier 3 needs one per platform, and the Windows one\n\
         is CI-able where this one is not."
    );
    std::process::exit(2);
}

mod known;

#[cfg(target_os = "macos")]
fn main() {
    print_expectations();
    macos::run();
}

/// Print the oracle before opening the window.
///
/// A harness can parse this, or a human can diff it against what S2 read. Either
/// way the expected answer exists in writing *before* anyone looks at a window,
/// which is the difference between a test target and a demo.
fn print_expectations() {
    println!("aibo-axtarget — deterministic AX target (plan §18 tier 3)\n");
    println!(
        "{:<22} {:>6} {:>6} {:>6} {:>10}  purpose",
        "identifier", "bytes", "utf16", "chars", "graphemes"
    );
    println!(
        "{:-<22} {:->6} {:->6} {:->6} {:->10}  {:-<40}",
        "", "", "", "", "", ""
    );
    for sample in known::ALL {
        let c = known::counts(sample.text);
        println!(
            "{:<22} {:>6} {:>6} {:>6} {:>10}  {}",
            sample.id, c.bytes, c.utf16, c.chars, c.graphemes, sample.purpose
        );
    }
    println!(
        "\nutf16 is the unit kAXSelectedTextRangeAttribute speaks (§8: an AXValue\n\
         wrapping CFRange). If a harness reports a range in any other unit, the\n\
         harness is wrong, not this app.\n"
    );
}

#[cfg(target_os = "macos")]
mod macos {
    use objc2::rc::Retained;
    use objc2::{MainThreadMarker, MainThreadOnly};
    use objc2_app_kit::{
        NSAccessibility, NSApplication, NSApplicationActivationPolicy, NSBackingStoreType,
        NSBorderType, NSScrollView, NSSecureTextField, NSTextField, NSTextView, NSView, NSWindow,
        NSWindowStyleMask,
    };
    use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

    use crate::known::{self, Sample};

    const WIDTH: f64 = 760.0;
    const MARGIN: f64 = 20.0;
    const ROW_HEIGHT: f64 = 26.0;
    const LABEL_HEIGHT: f64 = 18.0;
    const GAP: f64 = 10.0;
    const MULTI_HEIGHT: f64 = 190.0;

    /// Build the window and run the event loop. Never returns.
    pub fn run() {
        let mtm = MainThreadMarker::new()
            .expect("AppKit must be driven from the main thread — this is the main thread");

        let app = NSApplication::sharedApplication(mtm);
        // Regular, not Accessory: the target must be able to become frontmost
        // and hold key focus, which is the entire premise of the test.
        app.setActivationPolicy(NSApplicationActivationPolicy::Regular);

        // Total height: five rows, four of them label + control, the multi-line
        // one taller, plus margins.
        let height = MARGIN * 2.0
            + (LABEL_HEIGHT + ROW_HEIGHT + GAP) * 4.0
            + LABEL_HEIGHT
            + MULTI_HEIGHT
            + GAP;

        let window = new_window(mtm, WIDTH, height);
        let content = window
            .contentView()
            .expect("a titled NSWindow always has a content view");

        // AppKit's default coordinate system is bottom-left origin, so lay the
        // rows out from the top downwards by tracking a descending cursor.
        let mut cursor = height - MARGIN;

        for sample in [
            &known::SINGLE_LINE,
            &known::SINGLE_LINE_JA,
            &known::UNICODE_TRAPS,
        ] {
            cursor -= LABEL_HEIGHT;
            add_label(mtm, &content, sample, cursor);
            cursor -= ROW_HEIGHT;
            add_text_field(mtm, &content, sample, cursor);
            cursor -= GAP;
        }

        cursor -= LABEL_HEIGHT;
        add_label(mtm, &content, &known::SECURE, cursor);
        cursor -= ROW_HEIGHT;
        add_secure_field(mtm, &content, &known::SECURE, cursor);
        cursor -= GAP;

        cursor -= LABEL_HEIGHT;
        add_label(mtm, &content, &known::MULTI_LINE, cursor);
        cursor -= MULTI_HEIGHT;
        add_text_view(mtm, &content, &known::MULTI_LINE, cursor);

        window.makeKeyAndOrderFront(None);
        app.activate();

        println!("window up. Put the caret in a field, then run a spike against it.");
        println!("Quit with ⌘Q.");

        // `run` never returns; it is the AppKit event loop.
        app.run();
    }

    fn new_window(mtm: MainThreadMarker, width: f64, height: f64) -> Retained<NSWindow> {
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(width, height));
        let style = NSWindowStyleMask::Titled
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::Miniaturizable
            | NSWindowStyleMask::Resizable;

        // SAFETY: standard NSWindow designated initialiser, on the main thread,
        // with a valid frame and style mask.
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                frame,
                style,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        window.setTitle(&NSString::from_str("aibo AX target"));
        window.center();
        window
    }

    /// A non-editable `NSTextField` used as a caption.
    ///
    /// It is a real control, not a drawn string, so it also serves as a negative
    /// case: a harness that reports the *caption* as the focused editable field
    /// has a bug worth catching here rather than in Slack.
    fn add_label(mtm: MainThreadMarker, content: &NSView, sample: &Sample, y: f64) {
        let frame = NSRect::new(
            NSPoint::new(MARGIN, y),
            NSSize::new(WIDTH - MARGIN * 2.0, LABEL_HEIGHT),
        );
        let field = NSTextField::initWithFrame(NSTextField::alloc(mtm), frame);
        field.setStringValue(&NSString::from_str(&format!(
            "{}  [{}]",
            sample.label, sample.id
        )));
        field.setEditable(false);
        field.setSelectable(false);
        field.setBezeled(false);
        field.setDrawsBackground(false);
        field.setAccessibilityIdentifier(Some(&NSString::from_str(&format!(
            "{}.caption",
            sample.id
        ))));
        content.addSubview(&field);
    }

    fn add_text_field(mtm: MainThreadMarker, content: &NSView, sample: &Sample, y: f64) {
        let frame = NSRect::new(
            NSPoint::new(MARGIN, y),
            NSSize::new(WIDTH - MARGIN * 2.0, ROW_HEIGHT),
        );
        let field = NSTextField::initWithFrame(NSTextField::alloc(mtm), frame);
        field.setStringValue(&NSString::from_str(sample.text));
        field.setEditable(true);
        field.setSelectable(true);
        field.setBezeled(true);
        apply_accessibility(&field, sample);
        content.addSubview(&field);
    }

    fn add_secure_field(mtm: MainThreadMarker, content: &NSView, sample: &Sample, y: f64) {
        let frame = NSRect::new(
            NSPoint::new(MARGIN, y),
            NSSize::new(WIDTH - MARGIN * 2.0, ROW_HEIGHT),
        );
        let field = NSSecureTextField::initWithFrame(NSSecureTextField::alloc(mtm), frame);
        field.setEditable(true);
        field.setBezeled(true);
        field.setPlaceholderString(Some(&NSString::from_str(
            "focus here to turn on secure input mode",
        )));
        apply_accessibility(&field, sample);
        content.addSubview(&field);
    }

    fn add_text_view(mtm: MainThreadMarker, content: &NSView, sample: &Sample, y: f64) {
        let frame = NSRect::new(
            NSPoint::new(MARGIN, y),
            NSSize::new(WIDTH - MARGIN * 2.0, MULTI_HEIGHT),
        );
        let scroll = NSScrollView::initWithFrame(NSScrollView::alloc(mtm), frame);
        scroll.setHasVerticalScroller(true);
        scroll.setBorderType(NSBorderType::BezelBorder);

        let inner = NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(WIDTH - MARGIN * 2.0, MULTI_HEIGHT),
        );
        let view = NSTextView::initWithFrame(NSTextView::alloc(mtm), inner);
        view.setEditable(true);
        view.setSelectable(true);
        // Plain text only. Rich text would let AppKit substitute smart quotes and
        // dashes, and then the seed text on screen would not be the seed text in
        // `known.rs` — the fixture would silently stop being a fixture.
        view.setRichText(false);
        view.setAutomaticQuoteSubstitutionEnabled(false);
        view.setAutomaticDashSubstitutionEnabled(false);
        view.setAutomaticTextReplacementEnabled(false);
        view.setAutomaticSpellingCorrectionEnabled(false);

        view.setString(&NSString::from_str(sample.text));
        apply_accessibility(&view, sample);

        scroll.setDocumentView(Some(&view));
        content.addSubview(&scroll);
    }

    /// Set the two attributes a harness joins on.
    ///
    /// `accessibilityIdentifier` surfaces as `AXIdentifier` and is the stable
    /// key; `accessibilityLabel` surfaces as `AXDescription`, which §8 lists as
    /// the "field label" aibo puts in the Complete prompt (§5). Setting both
    /// means the target exercises the same two attributes the product reads.
    fn apply_accessibility(view: &NSView, sample: &Sample) {
        view.setAccessibilityIdentifier(Some(&NSString::from_str(sample.id)));
        view.setAccessibilityLabel(Some(&NSString::from_str(sample.label)));
    }
}
