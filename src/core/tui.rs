//! Utilities to render an interactive text-based user interface.
use cursive::theme::{Color, PaletteColor};
use cursive::{Cursive, CursiveRunnable, CursiveRunner};

/// Create an instance of a `CursiveRunner`, and clean it up afterward.
pub(crate) fn with_siv<T, F: FnOnce(CursiveRunner<CursiveRunnable>) -> anyhow::Result<T>>(
    f: F,
) -> anyhow::Result<T> {
    // I tried these back-ends:
    //
    // * `ncurses`/`pancurses`: Doesn't render ANSI escape codes. (NB: the fact
    //   that we print out strings with ANSI escape codes is tech debt; we would
    //   ideally pass around styled representations of all text, and decide how to
    //   rendering it later.) Rendered scroll view improperly. No mouse/scrolling
    //   support
    // * `termion`: Renders ANSI escape codes. Has mouse/scrolling support. But
    //   critical bug: https://github.com/gyscos/cursive/issues/563
    // * `crossterm`: Renders ANSI escape codes. Has mouse/scrolling support.
    //   However, has some flickering issues, particularly when scrolling. See
    //   issue at https://github.com/gyscos/cursive/issues/142. I tried the
    //   `cursive_buffered_backend` library, but this causes it to no longer
    //   respect the ANSI escape codes.
    // * `blt`: Seems to require that a certain library be present on the system
    //   for linking.
    let mut siv = cursive::crossterm();

    siv.update_theme(|theme| {
        theme.shadow = false;
        theme.palette.extend(vec![
            (PaletteColor::Background, Color::TerminalDefault),
            (PaletteColor::View, Color::TerminalDefault),
            (PaletteColor::Primary, Color::TerminalDefault),
        ]);
    });
    let old_max_level = log::max_level();
    log::set_max_level(log::LevelFilter::Off);
    let result = f(siv.into_runner());
    log::set_max_level(old_max_level);
    let result = result?;
    Ok(result)
}

/// Type-safe "singleton" view: a kind of view which is addressed by name, for
/// which exactly one copy exists in the Cursive application.
pub trait SingletonView<V> {
    /// Look up the instance of the singleton view in the application. Panics if
    /// it hasn't been added.
    fn find(siv: &mut Cursive) -> cursive::views::ViewRef<V>;
}

/// Create a set of views with unique names. See also `new_view!` and
/// `find_view!`.
///
/// ```
/// # use cursive::Cursive;
/// # use cursive::views::{EditView, TextView};
/// # use branchless::declare_views;
/// # use branchless::core::tui::SingletonView;
/// # fn main() {
/// declare_views! {
///     SomeDisplayView => TextView,
///     SomeDataEntryView => EditView,
/// }
/// let mut siv = Cursive::new();
/// siv.add_layer::<SomeDisplayView>(TextView::new("Hello, world!").into());
/// assert_eq!(SomeDisplayView::find(&mut siv).get_content().source(), "Hello, world!");
/// # }
/// ```
#[macro_export]
macro_rules! declare_views {
    { $( $k:ident => $v:ty ),* $(,)? } => {
        $(
            struct $k {
                view: cursive::views::NamedView<$v>,
            }

            impl $crate::core::tui::SingletonView<$v> for $k {
                fn find(siv: &mut Cursive) -> cursive::views::ViewRef<$v> {
                    siv.find_name::<$v>(stringify!($k)).unwrap()
                }
            }

            impl cursive::view::IntoBoxedView for $k {
                fn into_boxed_view(self) -> Box<dyn cursive::view::View> {
                    Box::new(self.view)
                }
            }

            impl From<$v> for $k {
                fn from(view: $v) -> Self {
                    use cursive::view::Nameable;
                    let view = view.with_name(stringify!($k));
                    $k { view }
                }
            }
        )*
    };
}

/// Testing helpers for interactive interfaces.
pub mod testing {
    use std::borrow::Borrow;
    use std::cell::RefCell;
    use std::rc::Rc;

    use cursive::backend::Backend;
    use cursive::theme::Color;

    /// Represents a "screenshot" of the terminal taken at a point in time.
    pub type Screen = Vec<Vec<char>>;

    /// The kind of events that can be
    #[derive(Clone, Debug)]
    pub enum CursiveTestingEvent {
        /// A regular Cursive event.
        Event(cursive::event::Event),

        /// Take a screenshot at the current point in time and store it in the
        /// provided screenshot cell.
        TakeScreenshot(Rc<RefCell<Screen>>),
    }

    /// The testing backend. It feeds a predetermined list of events to the
    /// Cursive event loop and stores a virtual terminal for Cursive to draw on.
    #[derive(Debug)]
    pub struct CursiveTestingBackend {
        events: Vec<CursiveTestingEvent>,
        event_index: usize,
        just_emitted_event: bool,
        screen: RefCell<Screen>,
        screenshots: Vec<Screen>,
    }

    impl<'screenshot> CursiveTestingBackend {
        /// Construct the testing backend with the provided set of events.
        pub fn init(events: Vec<CursiveTestingEvent>) -> Box<dyn Backend> {
            Box::new(CursiveTestingBackend {
                events,
                event_index: 0,
                just_emitted_event: false,
                screen: RefCell::new(vec![vec![' '; 120]; 24]),
                screenshots: Vec::new(),
            })
        }
    }

    impl Backend for CursiveTestingBackend {
        fn poll_event(&mut self) -> Option<cursive::event::Event> {
            // Cursive will poll all available events. We only want it to
            // process events one at a time, so return `None` after each event.
            if self.just_emitted_event {
                self.just_emitted_event = false;
                return None;
            }

            let event_index = self.event_index;
            self.event_index += 1;
            match self.events.get(event_index)?.to_owned() {
                CursiveTestingEvent::TakeScreenshot(screen_target) => {
                    let mut screen_target = (*screen_target).borrow_mut();
                    *screen_target = self.screen.borrow().clone();
                    self.poll_event()
                }
                CursiveTestingEvent::Event(event) => {
                    self.just_emitted_event = true;
                    Some(event)
                }
            }
        }

        fn refresh(&mut self) {}

        fn has_colors(&self) -> bool {
            false
        }

        fn screen_size(&self) -> cursive::Vec2 {
            let screen = self.screen.borrow();
            (screen[0].len(), screen.len()).into()
        }

        fn print_at(&self, pos: cursive::Vec2, text: &str) {
            for (i, c) in text.char_indices() {
                let mut screen = self.screen.borrow_mut();
                let screen_width = screen[0].len();
                if pos.x + i < screen_width {
                    screen[pos.y][pos.x + i] = c;
                } else {
                    // Indicate that the screen was overfull.
                    screen[pos.y][screen_width - 1] = '$';
                }
            }
        }

        fn clear(&self, _color: Color) {
            let mut screen = self.screen.borrow_mut();
            for i in 0..screen.len() {
                for j in 0..screen[i].len() {
                    screen[i][j] = ' ';
                }
            }
        }

        fn set_color(&self, colors: cursive::theme::ColorPair) -> cursive::theme::ColorPair {
            colors
        }

        fn set_effect(&self, _effect: cursive::theme::Effect) {}

        fn unset_effect(&self, _effect: cursive::theme::Effect) {}
    }

    /// Convert the screenshot into a string for assertions, such as for use
    /// with `insta::assert_snapshot!`.
    pub fn screen_to_string(screen: &Rc<RefCell<Screen>>) -> String {
        let screen = Rc::borrow(screen);
        let screen = RefCell::borrow(screen);
        screen
            .iter()
            .map(|row| {
                let line: String = row.iter().collect();
                line.trim().to_owned() + "\n"
            })
            .collect::<String>()
            .trim()
            .to_owned()
    }
}
