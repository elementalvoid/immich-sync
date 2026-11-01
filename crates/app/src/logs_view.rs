//! The Logs view (SPEC R10.3): where the log lives, the recent lines, the live
//! tail, the refresh action, and the persisted level control.
//!
//! The level is stored by the settings surface's store, because one file must
//! have one writer. Reading the file happens off the UI thread, on the way in
//! and then once a second while the Logs surface is on screen; a hidden surface
//! reads nothing.

use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use gpui_kit::base::{Disableable as _, SelectableText, TextSelection};
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::IndexPath;
use gpui_kit::component::button::Button;
use gpui_kit::component::scroll::{Scrollbar, ScrollbarMode};
use gpui_kit::component::searchable_list::{SearchableListDelegate, SearchableListItem};
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::*;

use crate::a11y;
use crate::dashboard::CardPalette;
use crate::empty_state::EmptyState;
use crate::logging::{self, LogLevel, TAIL_LINES};
use crate::notice::{Notice, Severity, copy_confirmation};
use crate::rows::{Row, mono_value_item, select_option, value_list};
use crate::settings_view::{SettingsView, filling_section_with_action, section};

/// How often the live tail re-reads the newest log file while the Logs surface
/// is visible.
pub const LIVE_TAIL_POLL: Duration = Duration::from_secs(1);

/// The Logs view.
pub struct LogsView {
    /// The settings surface owns the settings file and the level it persists.
    settings: Entity<SettingsView>,
    /// The directory the rotating log writes to (the state directory).
    directory: PathBuf,
    /// The bounded tail read when the view was shown, refreshed, or polled.
    lines: Vec<String>,
    /// The last read the paused count measured against. It advances with every
    /// read while the displayed rows stay frozen, so an idle poll cannot count
    /// the same arrival twice.
    counted: Vec<String>,
    /// The newest log file's name, read with the tail.
    current_file: Option<String>,
    /// Why the tail could not be read, when it could not.
    read_problem: Option<String>,
    /// Why the level could not be saved, when it could not.
    save_problem: Option<String>,
    /// Keeps the level notice in step with the settings surface's save
    /// failures, which now land after the worker write (N2).
    _subscriptions: Vec<Subscription>,
    /// Holds keyboard focus while the tail is dragged, which is what lets the
    /// Root's Ctrl+C binding reach the selection.
    lines_focus: FocusHandle,
    /// Where the tail is scrolled to. Following is this handle's distance from
    /// the bottom, never a line count.
    scroll: ScrollHandle,
    /// Where the cards above the tail are scrolled to. The strip only scrolls
    /// when the window is too short for them, and this is what its scrollbar
    /// draws while it does (#54).
    cards_scroll: ScrollHandle,
    /// Whether the user scrolled away from the newest lines, which pauses
    /// following. A poll sets it from the scroll handle, and a poll reads that
    /// handle after the frame that moved it, so a scroll this view asked for can
    /// never read as the user scrolling up.
    paused: bool,
    /// How many lines arrived while the user was reading older ones.
    new_lines: usize,
    /// The poll loop, running only while the surface is visible. Dropping it
    /// stops the timer.
    live: Option<Task<()>>,
    /// Whether the shell last marked the surface visible.
    visible: bool,
    /// Discards a read a newer one has superseded, so two reads in flight
    /// cannot count the same new lines twice.
    read_generation: u64,
    /// What the read in flight does when it lands. A read latches this before
    /// its file read starts, and the apply re-reads the viewport for a poll.
    follow: Follow,
    /// How many times Copy all has been pressed since the displayed tail last
    /// changed. The confirmation is one node whose announcement names the press,
    /// and it goes when the rows it copied are no longer the rows on screen
    /// (#82).
    copies: u32,
    /// The window the tail is painted in, where a selection is read from before
    /// a live replace would drop the rows under it. Set by every render.
    window: Option<AnyWindowHandle>,
}

/// What a tail read does when it lands.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Follow {
    /// Follow only while the tail is still scrolled to its end when the read
    /// lands, so a scroll during the read is not undone by the replace.
    FromViewport,
    /// Follow from the read on, whatever the viewport says: the user asked for
    /// the tail, and the jump control clears the pause while the rows still
    /// read as scrolled up.
    Always,
}

impl LogsView {
    /// A view over the log directory the settings paths resolved.
    ///
    /// It reads nothing until the shell shows its surface: [`Self::set_visible`]
    /// makes the first read, so a view that is never on screen never touches
    /// the log.
    pub fn new(settings: Entity<SettingsView>, cx: &mut Context<Self>) -> Self {
        let directory = settings.read(cx).paths().log_directory();
        let save_problem = settings.read(cx).save_problem().map(str::to_string);
        let subscription = cx.observe(&settings, |this, settings, cx| {
            this.save_problem = settings.read(cx).save_problem().map(str::to_string);
            cx.notify();
        });
        Self {
            settings,
            directory,
            lines: Vec::new(),
            counted: Vec::new(),
            current_file: None,
            read_problem: None,
            save_problem,
            _subscriptions: vec![subscription],
            lines_focus: cx.focus_handle(),
            scroll: ScrollHandle::new(),
            cards_scroll: ScrollHandle::new(),
            paused: false,
            new_lines: 0,
            live: None,
            visible: false,
            read_generation: 0,
            follow: Follow::Always,
            copies: 0,
            window: None,
        }
    }

    /// Starts or stops the live tail: the shell marks the surface visible when
    /// it shows the Logs view and not visible when it switches away.
    ///
    /// Showing reads once straight away, so an opened surface is current, and
    /// then once a [`LIVE_TAIL_POLL`]. A hidden surface has no loop at all, so
    /// nothing reads the file while another surface is on screen.
    pub fn set_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.visible == visible {
            return;
        }
        self.visible = visible;
        if !visible {
            self.live = None;
            return;
        }
        self.read_tail(Follow::Always, cx);
        self.live = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(LIVE_TAIL_POLL).await;
                if this.update(cx, |this, cx| this.poll_live(cx)).is_err() {
                    break;
                }
            }
        }));
    }

    /// Whether the live tail is running, which the shell decides through
    /// [`Self::set_visible`].
    #[must_use]
    pub fn is_live(&self) -> bool {
        self.live.is_some()
    }

    /// One tick of the live tail: reads where the tail is scrolled to, then
    /// re-reads the file for as long as the surface is visible.
    ///
    /// Following is the scroll position, so the user returning to the end
    /// resumes it and scrolling up pauses it, whatever moved the handle. An
    /// existing selection pauses it too: a live replace would overwrite the
    /// text under it (#38's defect on the hot path).
    fn poll_live(&mut self, cx: &mut Context<Self>) {
        if !self.visible {
            return;
        }
        self.paused = !self.at_bottom() || self.has_selection(cx);
        self.read_tail(Follow::FromViewport, cx);
    }

    /// Whether the window holds any text selection. The check is window-wide,
    /// the same scope Refresh's clear uses.
    fn has_selection(&self, cx: &mut Context<Self>) -> bool {
        self.window.is_some_and(|window| {
            window
                .update(cx, |_, window, cx| TextSelection::has_selection(window, cx))
                // A window that cannot be read may hold a selection, and a
                // replace over one loses the user's text: pause instead.
                .unwrap_or(true)
        })
    }

    /// Re-reads the newest log file's name and the tail of its lines, follows
    /// it again, and drops any selection the old tail held so Ctrl+C cannot
    /// copy text that is no longer on screen (#38).
    pub fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // The clear is window-wide rather than scoped to this view, so a
        // refresh also drops a selection held in another surface's text. It is
        // acceptable here: Refresh is the user's own action on the one text
        // surface whose rows it replaces, and a selection left over rows that
        // no longer hold that text is the bug #38 fixed.
        TextSelection::clear(window, cx);
        self.resume(cx);
    }

    /// Takes the current tail and follows it again. Refresh and the jump
    /// control both ask for that.
    fn resume(&mut self, cx: &mut Context<Self>) {
        self.paused = false;
        self.read_tail(Follow::Always, cx);
    }

    /// Reads the newest file's name and its bounded tail on the background
    /// executor (N2), then applies the result on the UI thread. `follow` says
    /// what the apply does with a viewport the read did not wait for.
    fn read_tail(&mut self, follow: Follow, cx: &mut Context<Self>) {
        self.follow = follow;
        self.read_generation = self.read_generation.wrapping_add(1);
        let generation = self.read_generation;
        let directory = self.directory.clone();
        let read = cx
            .background_executor()
            .spawn(async move { logging::newest_tail(&directory, TAIL_LINES) });
        cx.spawn(async move |this, cx| {
            let read = read.await;
            let _ = this.update(cx, |this, cx| this.apply_tail(generation, read, cx));
        })
        .detach();
    }

    /// Applies one tail read: replaces the tail while the view follows it, and
    /// only counts the new lines while the user reads older ones.
    fn apply_tail(
        &mut self,
        generation: u64,
        read: anyhow::Result<(Option<String>, Vec<String>)>,
        cx: &mut Context<Self>,
    ) {
        // A read still in flight when the surface hides changes nothing.
        if !self.visible {
            return;
        }
        // A newer read supersedes an older one, so a slow read cannot land over
        // it or count the same lines twice.
        if generation != self.read_generation {
            return;
        }
        let (file, tail) = match read {
            Ok(read) => read,
            Err(error) => {
                // The lines already on screen stay: a read that failed says so
                // and a later poll can recover them.
                self.read_problem = Some(format!("Could not read the log file: {error:#}"));
                cx.notify();
                return;
            }
        };
        self.read_problem = None;
        // The timer's read decided before the file read finished, and the user
        // may have scrolled or selected since: replacing now would pull them
        // back to the end or drop the text under the selection. A read the user
        // asked for follows whatever the viewport says.
        if self.follow == Follow::FromViewport && (!self.at_bottom() || self.has_selection(cx)) {
            self.paused = true;
        }
        if self.paused {
            self.new_lines = self
                .new_lines
                .saturating_add(gained_lines(&self.counted, &tail));
            // The counted window advances while the rows stay frozen, so the
            // next poll measures what arrived after this read. The current
            // file's name stays with the rows: it names the file they show.
            self.counted = tail;
        } else {
            // The confirmation names the rows it copied, so it goes as soon as
            // they are not the rows on screen. A poll that read the same tail
            // leaves it alone, so the live tail cannot make it flicker (#82).
            if self.lines != tail {
                self.copies = 0;
            }
            self.current_file = file;
            self.lines = tail.clone();
            self.counted = tail;
            self.new_lines = 0;
            self.scroll.scroll_to_bottom();
        }
        cx.notify();
    }

    /// Whether the tail is scrolled to its end.
    ///
    /// The offset runs from zero at the top to `-max_offset` at the bottom, so
    /// `max_offset + offset` is the distance left below the last line. A few
    /// pixels of slack keep the sub-pixel layout from reading as a scroll up.
    fn at_bottom(&self) -> bool {
        f32::from(self.scroll.max_offset().y + self.scroll.offset().y) < 4.
    }

    /// The level the settings hold, which is also the level the subscriber runs
    /// with.
    #[must_use]
    pub fn level(&self, cx: &App) -> LogLevel {
        self.settings.read(cx).log_level()
    }

    /// Persists the level and applies it to the running subscriber. The
    /// settings surface reports the outcome through its own subscription.
    fn set_level(&mut self, level: LogLevel, cx: &mut Context<Self>) {
        self.settings
            .update(cx, |settings, cx| settings.set_log_level(level, cx));
    }

    /// Copies the whole displayed tail, one line per line, in order.
    ///
    /// The clipboard is written from the lines the view holds, so nothing that
    /// was not written to the log can reach it. Joining keeps blank lines,
    /// which the selection layer's copy path drops. The confirmation is the
    /// copy's own: a refused copy sets nothing and confirms nothing (#82).
    fn copy_all(&mut self, cx: &mut Context<Self>) {
        if self.lines.is_empty() {
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(self.lines.join("\n")));
        self.copies += 1;
        cx.notify();
    }

    /// The page header.
    fn header(cx: &App) -> impl IntoElement + use<> {
        let muted = cx.theme().muted_foreground;
        div()
            .flex()
            .flex_col()
            .gap_0p5()
            .child(
                div()
                    .text_2xl()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Logs"),
            )
            .child(div().text_sm().text_color(muted).child(
                "The client writes a rotating log file here. Passwords and API keys never reach it.",
            ))
    }

    /// The log location.
    fn location(&self, cx: &mut Context<Self>) -> AnyElement {
        let palette = CardPalette::new(cx.theme());
        let mono = cx.theme().mono_font_family.clone();
        let current = self
            .current_file
            .clone()
            .unwrap_or_else(|| "No log file yet.".to_string());
        // The two paths are read-only pairs, not label-and-control rows: the
        // term column carries the label and the value column the path and its
        // help, so the path wraps instead of truncating (#80's shape).
        let paths = value_list(
            "logs-path-values",
            vec![
                mono_value_item(
                    "Log folder",
                    Some("Resolved from the platform data directory."),
                    "logs-path-directory-value",
                    self.directory.display().to_string(),
                    &palette,
                    mono.clone(),
                ),
                mono_value_item(
                    "Current file",
                    Some(
                        "Reads from the newest file. It follows the file while this surface is open.",
                    ),
                    "logs-path-current-value",
                    current,
                    &palette,
                    mono,
                ),
            ],
        );
        // Refresh is not a row of this card: it shares the level control's row
        // below, which is one row fewer between the header and the tail.
        section(
            "logs-location",
            "Log file",
            "Same directory as the state database. The file rotates and old files are pruned.",
            vec![paths.into_any_element()],
            cx,
        )
    }

    /// The level control and the Refresh action, on one row: the level control
    /// on the left and Refresh at the row's right end.
    ///
    /// The change applies immediately and persists.
    fn level_section(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let current = self.level(cx);
        let select = self.level_select(current, window, cx);
        let choices = select.read(cx).choices.clone();
        let refresh = Rc::new(cx.listener(|this, _, window, cx| this.refresh(window, cx)));
        let mut rows = Vec::with_capacity(2);
        if let Some(problem) = &self.save_problem {
            rows.push(
                Notice::new("logs-level-problem", Severity::Error, problem.clone())
                    .into_any_element(),
            );
        }
        rows.push(
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .items_center()
                .justify_between()
                .gap_3()
                .w_full()
                .child(
                    div().w_48().flex_shrink_0().child(
                        Select::new(&choices)
                            .id("logs-level-select")
                            .accessibility_label("Log level")
                            .placeholder(current.label()),
                    ),
                )
                // #89's row id and name stay on the control's group, so the
                // migrated-row table still finds what it pins. The help
                // sentence is gone: Refresh says what it does.
                .child(
                    a11y::named(Role::Group, "logs-refresh-row", "Refresh", None)
                        .flex_shrink_0()
                        .child(
                            Button::new("logs-refresh")
                                .label("Refresh")
                                .accessibility_label("Refresh the log lines")
                                .on_click(move |event, window, cx| refresh(event, window, cx)),
                        ),
                )
                .into_any_element(),
        );
        section(
            "logs-level",
            "Log level",
            "How much detail the file holds. The change applies immediately and is saved.",
            rows,
            cx,
        )
    }

    /// The level select, kept in step with the level the settings hold.
    ///
    /// Keyed element state, the shape #79 gave the folder filter: the settings
    /// stay the owner of the choice, and every way of changing it (a chosen
    /// option, a save that failed) is pushed into the control on the next frame.
    fn level_select(
        &self,
        current: LogLevel,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<LevelSelect> {
        let view = cx.entity().downgrade();
        let binding = window.use_keyed_state(
            ElementId::Name("logs-level-select-state".into()),
            cx,
            move |window, cx| {
                let choices = LevelChoices;
                let chosen = choices.index_of(current);
                let state = cx.new(|cx| SelectState::new(choices, chosen, window, cx));
                let confirmed = cx.subscribe(
                    &state,
                    move |this: &mut LevelSelect,
                          _: Entity<SelectState<LevelChoices>>,
                          event: &SelectEvent<LevelChoices>,
                          cx| {
                        let SelectEvent::Confirm(value) = event;
                        let Some(level) = value else {
                            // The kit's own clear button, which this control does
                            // not show: a log always has a level.
                            return;
                        };
                        this.chosen = *level;
                        _ = view.update(cx, |view, cx| view.set_level(*level, cx));
                    },
                );
                LevelSelect {
                    choices: state,
                    chosen: current,
                    _confirmed: confirmed,
                }
            },
        );
        binding.update(cx, |this, cx| this.sync(current, window, cx));
        binding
    }

    /// The bounded tail of the newest log file.
    fn lines_section(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let palette = CardPalette::new(cx.theme());
        let mono = cx.theme().mono_font_family.clone();
        let mut body: Vec<AnyElement> = Vec::with_capacity(3);
        if let Some(problem) = &self.read_problem {
            body.push(
                div()
                    // The message quotes the log directory, so it wraps further
                    // on a long path. Clamped to the two lines the floor's 4rem
                    // covers; the accessible label keeps the whole text (#54).
                    .line_clamp(2)
                    .child(Notice::new(
                        "logs-problem",
                        Severity::Error,
                        problem.clone(),
                    ))
                    .into_any_element(),
            );
        }
        if self.lines.is_empty() {
            // #77: the tail's empty state, and the one step it can carry out
            // from where the reader notices it is empty.
            body.push(
                EmptyState::new(
                    "logs-empty",
                    "No log lines yet.",
                    "Lines appear as the client works.",
                )
                .action(
                    Button::new("logs-empty-refresh")
                        .label("Refresh")
                        // Its own name: at an empty tail both refreshes are
                        // on screen and must not sound alike.
                        .accessibility_label("Refresh the log tail")
                        .on_click(cx.listener(|this, _, window, cx| this.refresh(window, cx))),
                )
                .into_any_element(),
            );
        } else {
            let lines: Vec<AnyElement> = self
                .lines
                .iter()
                .enumerate()
                .map(|(index, line)| {
                    crate::a11y::named(
                        Role::Label,
                        ElementId::Name(format!("logs-line-{index}").into()),
                        line.clone(),
                        None,
                    )
                    .min_w_0()
                    .w_full()
                    // The tail box caps its height, so a shrinkable row is
                    // squeezed into the cap and its glyphs are clipped: the
                    // tail must overflow and scroll instead (#38).
                    .flex_shrink_0()
                    .truncate()
                    .font_family(mono.clone())
                    .text_xs()
                    .text_color(palette.text)
                    // One handle per line: a shared handle registers once
                    // and the last line painted would win (probe Q1).
                    // A blank line selected by a drag is dropped from the
                    // copied text: the selection layer drops whitespace-only
                    // participants and the Root trims. That is vendored
                    // gpui-base behaviour; Copy all keeps blank lines.
                    .child(
                        SelectableText::new(
                            ElementId::Name(format!("logs-text-{index}").into()),
                            line.clone(),
                        )
                        .document_order(index as u64),
                    )
                    .into_any_element()
                })
                .collect();
            let focus = self.lines_focus.clone();
            body.push(
                // The scrollbar overlays this row rather than the scroll box, so
                // it stays put while the lines move. It reads the view's own
                // handle: following is that handle's distance from the bottom.
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h(rems(8.))
                    .relative()
                    .child(
                        div()
                            .id("logs-lines")
                            .test_support()
                            .track_focus(&self.lines_focus)
                            .track_scroll(&self.scroll)
                            .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                                window.focus(&focus, cx);
                            })
                            // The wheel over the tail is the tail's: GPUI applies
                            // the delta to every scroll hitbox under the pointer,
                            // so without this any scrolling ancestor would move
                            // by the same delta (#54).
                            .on_scroll(|_, _, cx| cx.stop_propagation())
                            .flex()
                            .flex_col()
                            .gap_0p5()
                            .size_full()
                            // Room for the scrollbar track over the right edge,
                            // so it cannot cover a truncated line's ending.
                            .pr_4()
                            .overflow_y_scroll()
                            .children(lines)
                            .into_any_element(),
                    )
                    .child(
                        // Always: the bar shows while the lines overflow
                        // and hides only because they fit, not two seconds
                        // after the last wheel (#54). It reads the view's
                        // own handle, so the drag and the wheel write the
                        // same offset that following measures. The bar lays
                        // itself out over the relative box above.
                        Scrollbar::vertical(&self.scroll)
                            .mode(ScrollbarMode::Always)
                            .viewport_from_layout(),
                    )
                    .into_any_element(),
            );
        }
        // Below the tail, so pausing and resuming cannot move the lines the
        // user is reading.
        if self.paused {
            let jump = match self.new_lines {
                0 => "Jump to latest".to_string(),
                1 => "1 new line · Jump to latest".to_string(),
                count => format!("{count} new lines · Jump to latest"),
            };
            let resume = Rc::new(cx.listener(|this, _, _, cx| this.resume(cx)));
            body.push(
                Row::wrapped(
                    "Live tail paused",
                    "New lines keep arriving; the tail stays where you left it.",
                    "logs-paused",
                    move |_, _| {
                        let resume = Rc::clone(&resume);
                        Button::new("logs-jump-latest")
                            .label(jump.clone())
                            .accessibility_label(jump.clone())
                            .on_click(move |event, window, cx| resume(event, window, cx))
                            .into_any_element()
                    },
                )
                .render(window, cx),
            );
        }
        // The read notice and the paused row each sit inside the card beside
        // the tail, and each wraps at the narrowest window, so the floor covers
        // them too: 16rem, plus 4rem for every extra row the body shows (#54).
        let floor_rem = 16.
            + if self.read_problem.is_some() { 4. } else { 0. }
            + if self.paused { 4. } else { 0. };
        filling_section_with_action(
            "logs-lines-section",
            "Recent lines",
            "The newest file's last lines. They follow the file while this surface is open.",
            self.copy_all_action(cx),
            body,
            floor_rem,
            cx,
        )
    }

    /// Copy all, at the right end of the Recent lines header.
    ///
    /// It costs no row of its own, and #82's confirmation still sits beside the
    /// control it belongs to: the header is wide enough for both at the shell's
    /// 520px minimum (the button and the confirmation take ~180px of it).
    fn copy_all_action(&self, cx: &mut Context<Self>) -> AnyElement {
        let copy_all = Rc::new(cx.listener(|this, _, _, cx| this.copy_all(cx)));
        let nothing_to_copy = self.lines.is_empty();
        let copies = self.copies;
        // #89's row id and name stay on the control's group, so the
        // migrated-row table still finds what it pins. The help sentence is
        // gone: Copy all says what it does.
        a11y::named(Role::Group, "logs-copy-row", "Copy all", None)
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .children(copy_confirmation("logs-copy-confirmation", copies))
            .child(
                Button::new("logs-copy-all")
                    .label("Copy all")
                    .accessibility_label("Copy every displayed log line")
                    .disabled(nothing_to_copy)
                    .on_click(move |event, window, cx| copy_all(event, window, cx)),
            )
            .into_any_element()
    }
}

/// One level select's state: the kit state and the level it last read from the
/// settings.
///
/// Keyed element state, so the view does not own the control's state and the
/// control survives a re-render the way a widget's does.
struct LevelSelect {
    choices: Entity<SelectState<LevelChoices>>,
    /// The level the control last showed, so a frame that changed nothing does
    /// not push a value back into the kit state (a setter notifies, so doing
    /// it every frame schedules a frame every frame).
    chosen: LogLevel,
    _confirmed: Subscription,
}

impl LevelSelect {
    /// Pushes the level the settings hold into the control when it differs.
    fn sync(&mut self, current: LogLevel, window: &mut Window, cx: &mut Context<Self>) {
        if self.chosen != current {
            self.chosen = current;
            let value = current;
            self.choices.update(cx, |state, cx| {
                state.set_selected_value(&value, window, cx);
            });
        }
    }
}

/// The level select's items: the five levels, in the order the control shows
/// them.
struct LevelChoices;

impl LevelChoices {
    /// The row that holds `level`, so the control opens on the current choice.
    fn index_of(&self, level: LogLevel) -> Option<IndexPath> {
        LogLevel::ALL
            .iter()
            .position(|held| *held == level)
            .map(IndexPath::new)
    }
}

impl SearchableListDelegate for LevelChoices {
    type Item = LogLevel;

    fn items_count(&self, section: usize) -> usize {
        if section == 0 { LogLevel::ALL.len() } else { 0 }
    }

    fn item(&self, ix: IndexPath) -> Option<&Self::Item> {
        LogLevel::ALL.get(ix.row)
    }

    fn position<V>(&self, value: &V) -> Option<IndexPath>
    where
        Self::Item: SearchableListItem<Value = V>,
        V: PartialEq,
    {
        LogLevel::ALL
            .iter()
            .position(|level| level.value() == value)
            .map(IndexPath::new)
    }

    /// The row the kit paints for each level: [`select_option`], with the
    /// level's own id, so the option a state change lands on is named after the
    /// setting it holds.
    fn render_item(
        &self,
        _ix: IndexPath,
        item: &Self::Item,
        checked: bool,
        _window: &mut Window,
        cx: &mut App,
    ) -> Option<AnyElement> {
        Some(select_option(item.id(), item.label(), checked, cx))
    }
}

impl SearchableListItem for LogLevel {
    type Value = Self;

    fn title(&self) -> SharedString {
        self.label().into()
    }

    fn value(&self) -> &Self::Value {
        self
    }
}

/// How many lines `fresh` holds that `held` does not.
///
/// The file only grows, so the tail on screen is a run at the start of the
/// fresh one and everything after that run is new. A whole fresh window means
/// the run is gone, which happens when the log rotated or outgrew the window.
fn gained_lines(held: &[String], fresh: &[String]) -> usize {
    let longest = held.len().min(fresh.len());
    let shared = (1..=longest)
        .rev()
        .find(|count| held[held.len() - count..] == fresh[..*count])
        .unwrap_or(0);
    fresh.len() - shared
}

impl Render for LogsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The live poll reads the window's selection before it replaces the
        // rows, and this is where a window reference is available.
        self.window = Some(window.window_handle());
        let theme = cx.theme();
        let background = theme.background;
        let foreground = theme.foreground;
        let location = self.location(cx);
        let level = self.level_section(window, cx);
        let lines = self.lines_section(window, cx);

        div()
            .id("logs-view")
            .test_support()
            .flex()
            .flex_col()
            .size_full()
            .bg(background)
            .text_color(foreground)
            // Clipped, so the page keeps the window's height for the tail to
            // divide instead of growing with the cards' content.
            .overflow_hidden()
            .p_4()
            .gap_2()
            .child(
                // The header, the location and the level control keep their
                // natural height, and the tail takes what is left of the
                // window. A window too short for both scrolls this strip rather
                // than squeezing the tail away; a page-level scroll could not
                // do it, because a column inside a scroll area has no height to
                // divide (#54).
                div()
                    .id("logs-cards")
                    .test_support()
                    .flex()
                    .flex_col()
                    .min_h_0()
                    .relative()
                    .child(
                        div()
                            .id("logs-card-list")
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_h_0()
                            .gap_2()
                            .track_scroll(&self.cards_scroll)
                            .overflow_y_scroll()
                            .child(Self::header(cx))
                            .child(location)
                            .child(level),
                    )
                    .child(
                        // The strip is the fallback scroll at a short window,
                        // so it needs the affordance too (#54).
                        Scrollbar::vertical(&self.cards_scroll)
                            .mode(ScrollbarMode::Always)
                            .viewport_from_layout(),
                    ),
            )
            .child(lines)
    }
}
