//! The dashboard view (SPEC R8.1): a global status card and one card per
//! source folder.
//!
//! Keyboard navigation stays out until the form controls land in #3.

use crate::engine::EngineModel;
use crate::status::{ActivityState, DashboardModel, StatusCard};

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::Colorize as _;
use gpui_kit::component::Theme;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::progress::Progress;
use gpui_kit::component::radio::Radio;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::tag::Tag;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use sync_core::store::AlbumScope;

/// The dashboard view: a status card per source folder, the global card, and
/// the folder controls (R8.1).
///
/// With an [`EngineModel`] the view renders the live engine status and sends
/// every control straight to the engine. Without one (the layout tests) it
/// renders the injected model and keeps the pause state locally.
pub struct Dashboard {
    model: DashboardModel,
    engine: Option<Entity<EngineModel>>,
    folder_input: Option<Entity<InputState>>,
    paused: bool,
    _subscriptions: Vec<Subscription>,
}

impl Dashboard {
    /// Builds the dashboard from a fixed view model, with no engine attached.
    #[must_use]
    pub fn new(model: DashboardModel) -> Self {
        Self {
            model,
            engine: None,
            folder_input: None,
            paused: false,
            _subscriptions: Vec::new(),
        }
    }

    /// Builds the live dashboard over the app-scoped engine (R8.1).
    pub fn with_engine(
        engine: Entity<EngineModel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let folder_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("/home/you/Pictures")
                .masked(false)
        });
        let mut subscriptions = Vec::with_capacity(2);
        subscriptions.push(cx.observe(&engine, |this, engine, cx| {
            this.model = engine.read(cx).dashboard_model(cx);
            cx.notify();
        }));
        subscriptions.push(cx.subscribe_in(
            &folder_input,
            window,
            |this, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.add_folder(window, cx);
                }
            },
        ));
        let model = engine.read(cx).dashboard_model(cx);
        Self {
            model,
            engine: Some(engine),
            folder_input: Some(folder_input),
            paused: false,
            _subscriptions: subscriptions,
        }
    }

    /// Whether new uploads are held (R8.7).
    fn is_paused(&self, cx: &App) -> bool {
        match &self.engine {
            Some(engine) => engine.read(cx).is_paused(),
            None => self.paused,
        }
    }

    fn sync_now(&mut self, cx: &mut Context<Self>) {
        match &self.engine {
            Some(engine) => engine.update(cx, |model, cx| model.sync_now(cx)),
            None => {
                self.paused = false;
                self.model.global.activity = ActivityState::Syncing;
                cx.notify();
            }
        }
    }

    fn toggle_paused(&mut self, cx: &mut Context<Self>) {
        match &self.engine {
            Some(engine) => {
                let paused = engine.read(cx).is_paused();
                engine.update(cx, |model, cx| {
                    if paused {
                        model.resume(cx);
                    } else {
                        model.pause(cx);
                    }
                });
                self.model = engine.read(cx).dashboard_model(cx);
                cx.notify();
            }
            None => {
                self.paused = !self.paused;
                self.model.global.activity = if self.paused {
                    ActivityState::Paused
                } else {
                    ActivityState::Syncing
                };
                cx.notify();
            }
        }
    }

    /// Adds the path the field holds (R2.1).
    fn add_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(input) = self.folder_input.clone() else {
            return;
        };
        let path = input.read(cx).value().trim().to_string();
        if let Some(engine) = &self.engine {
            engine.update(cx, |model, cx| model.add_folder(path, true, cx));
        }
        input.update(cx, |state, cx| state.set_value("", window, cx));
    }

    /// Removes one source folder (R2.1).
    fn remove_folder(&mut self, path: String, cx: &mut Context<Self>) {
        if let Some(engine) = &self.engine {
            engine.update(cx, |model, cx| model.remove_folder(path, cx));
        }
    }

    /// Turns recursive discovery on or off for one source folder (R2.1).
    ///
    /// The engine model treats this as the folder's own edit, updates the
    /// existing folder, and re-watches and rescans it.
    fn set_folder_recursive(&mut self, path: String, recursive: bool, cx: &mut Context<Self>) {
        if let Some(engine) = &self.engine {
            engine.update(cx, |model, cx| model.add_folder(path, recursive, cx));
        }
    }

    /// Turns album mapping on or off for one source folder (R3.4).
    fn set_album_mapping(&mut self, path: String, enabled: bool, cx: &mut Context<Self>) {
        if let Some(engine) = &self.engine {
            engine.update(cx, |model, cx| model.set_album_mapping(path, enabled, cx));
        }
    }

    /// Sets the album scope for one source folder (R3.4).
    fn set_album_scope(&mut self, path: String, scope: AlbumScope, cx: &mut Context<Self>) {
        if let Some(engine) = &self.engine {
            engine.update(cx, |model, cx| model.set_album_scope(path, scope, cx));
        }
    }

    /// Shows the connection state on the global card (SPEC R8.1).
    pub fn set_connection(&mut self, state: Option<String>, cx: &mut Context<Self>) {
        if self.model.global.connection != state {
            self.model.global.connection = state;
            cx.notify();
        }
    }

    /// The folder add field and its button (R2.1).
    fn folder_bar(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let muted = cx.theme().muted_foreground;
        let input = self.folder_input.clone();
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(
                div()
                    .text_sm()
                    .text_color(muted)
                    .child("Add a folder to sync"),
            )
            .when_some(input, |this, input| {
                let focus = input.read(cx).focus_handle(cx);
                this.child(
                    div()
                        .id("dashboard-folder-path")
                        .test_support()
                        .track_focus(&focus)
                        .aria_label("Source folder path")
                        .flex_1()
                        .min_w_0()
                        .child(Input::new(&input).aria_label("Source folder path")),
                )
                .child(
                    Button::new("dashboard-add-folder")
                        .primary()
                        .label("Add folder")
                        .accessibility_label("Add folder")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.add_folder(window, cx);
                        })),
                )
            })
    }

    /// The recursion switch, the album mapping switch, and the album scope
    /// choice for one folder card (R2.1, R3.4).
    ///
    /// The scope stays choosable while the mapping is off: it is the folder's
    /// stored scope (R3.4) and takes effect as soon as the mapping is on, and
    /// the kit's `Radio` does not announce a disabled state, so a disabled
    /// radio would read as an enabled one (N5).
    fn folder_controls(
        &self,
        index: usize,
        card: &StatusCard,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let muted = cx.theme().muted_foreground;
        let title = card.title.clone();
        let path = card.path.clone().unwrap_or_default();
        let leaf = card.album_scope == AlbumScope::Leaf;
        div()
            .flex()
            .flex_row()
            // The card is narrow; the controls wrap instead of clipping.
            .flex_wrap()
            .items_center()
            .gap_2()
            .child(div().text_xs().text_color(muted).child("Recursive"))
            .child(
                Switch::new(("folder-recursive", index))
                    .checked(card.recursive)
                    .accessibility_label(format!("Sync {title} recursively"))
                    .on_click(cx.listener({
                        let path = path.clone();
                        move |this, recursive, _, cx| {
                            this.set_folder_recursive(path.clone(), *recursive, cx);
                        }
                    })),
            )
            .child(div().text_xs().text_color(muted).child("Album mapping"))
            .child(
                Switch::new(("folder-album", index))
                    .checked(card.album_mapping)
                    .accessibility_label(format!("Album mapping for {title}"))
                    .on_click(cx.listener({
                        let path = path.clone();
                        move |this, enabled, _, cx| {
                            this.set_album_mapping(path.clone(), *enabled, cx);
                        }
                    })),
            )
            .child(
                Radio::new(("folder-album-leaf", index))
                    .label(AlbumScope::Leaf.label())
                    .accessibility_label(format!("Album scope for {title}: one album per folder"))
                    .checked(leaf)
                    .on_change(cx.listener({
                        let path = path.clone();
                        move |this, checked, _, cx| {
                            if *checked {
                                this.set_album_scope(path.clone(), AlbumScope::Leaf, cx);
                            }
                        }
                    })),
            )
            .child(
                Radio::new(("folder-album-whole", index))
                    .label(AlbumScope::WholeFolder.label())
                    .accessibility_label(format!(
                        "Album scope for {title}: one album for this folder"
                    ))
                    .checked(!leaf)
                    .on_change(cx.listener({
                        let path = path.clone();
                        move |this, checked, _, cx| {
                            if *checked {
                                this.set_album_scope(path.clone(), AlbumScope::WholeFolder, cx);
                            }
                        }
                    })),
            )
    }

    fn header(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let muted = cx.theme().muted_foreground;
        let paused = self.is_paused(cx);
        let (pause_label, pause_name) = if paused {
            ("Resume", "Resume syncing")
        } else {
            ("Pause", "Pause syncing")
        };

        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap_4()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .child(
                        div()
                            .text_2xl()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Immich Sync"),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(muted)
                            .child("Upload status for every source folder"),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .child(
                        Button::new("dashboard-sync-now")
                            .primary()
                            .label("Sync now")
                            .accessibility_label("Sync now")
                            .on_click(cx.listener(|this, _, _, cx| this.sync_now(cx))),
                    )
                    .child(
                        Button::new("dashboard-pause")
                            .secondary()
                            .label(pause_label)
                            .accessibility_label(pause_name)
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_paused(cx))),
                    ),
            )
    }
}

impl Render for Dashboard {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let background = cx.theme().background;
        let foreground = cx.theme().foreground;
        let muted = cx.theme().muted_foreground;
        let header = self.header(cx);

        let folder_bar = self.folder_bar(cx);
        let removed = self.engine.is_some();

        div()
            .id("dashboard-scroll")
            .test_support()
            .flex()
            .flex_col()
            .size_full()
            .bg(background)
            .text_color(foreground)
            // A short window scrolls instead of hiding the last card's controls
            // (R8.1: every card stays reachable).
            .overflow_y_scroll()
            .p_6()
            .gap_4()
            .child(header)
            .child(status_card(
                "status-global".into(),
                "status-state-global".into(),
                Some("status-connection-global".into()),
                &self.model.global,
                None,
                None,
                cx,
            ))
            .child(folder_bar)
            .child(div().text_sm().text_color(muted).child("Source folders"))
            .child(
                div().flex().flex_row().flex_wrap().gap_4().children(
                    self.model
                        .folders
                        .iter()
                        .enumerate()
                        .map(|(index, folder)| {
                            let remove = removed.then(|| {
                                let path = folder.path.clone().unwrap_or_default();
                                let title = folder.title.clone();
                                Button::new(("folder-remove", index))
                                    .ghost()
                                    .label("Remove")
                                    .accessibility_label(format!("Remove {title}"))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.remove_folder(path.clone(), cx);
                                    }))
                                    .into_any_element()
                            });
                            let album = removed.then(|| {
                                self.folder_controls(index, folder, cx).into_any_element()
                            });
                            div().w(rems(17.5)).flex().child(status_card(
                                ("status-folder", index).into(),
                                ("status-state-folder", index).into(),
                                None,
                                folder,
                                album,
                                remove,
                                cx,
                            ))
                        }),
                ),
            )
    }
}

/// The theme tokens one dashboard card paints with.
///
/// The color choice lives here, so the WCAG contrast test measures the tokens
/// the view actually paints rather than a copy of them.
#[derive(Clone, Copy, Debug)]
pub struct CardPalette {
    /// The card fill.
    pub surface: Hsla,
    /// The card title and notice text.
    pub text: Hsla,
    /// The path, percentage, and counts text.
    pub meta: Hsla,
    /// The state pill fill.
    pub pill_fill: Hsla,
    /// The state pill text.
    pub pill_text: Hsla,
}

impl CardPalette {
    /// Reads the palette from the active theme.
    #[must_use]
    pub fn new(theme: &Theme) -> Self {
        Self {
            surface: theme.popover,
            text: theme.foreground,
            meta: theme.muted_foreground,
            pill_fill: theme.secondary,
            pill_text: theme.secondary_foreground,
        }
    }
}

/// How far a state hue is darkened to read as a 1px border on a card.
///
/// The kit's light accents are too bright for a border: the light `info` and
/// `warning` tokens sit below the WCAG non-text contrast floor of 3:1 on the
/// card surface. This factor clears it in both themes.
const STATE_BORDER_DARKEN: f32 = 0.25;

/// The border color that carries one activity state's hue.
///
/// The pill label stays in the theme's paired pill ink, because the light
/// accents are too bright to read as text on a light card.
#[must_use]
pub fn state_border(state: ActivityState, theme: &Theme) -> Hsla {
    let accent = match state {
        ActivityState::Idle => theme.muted_foreground,
        ActivityState::Syncing => theme.info,
        ActivityState::Paused => theme.warning,
        ActivityState::NeedsAttention => theme.danger,
    };
    accent.darken(STATE_BORDER_DARKEN)
}

/// One status card. The text comes from [`StatusCard::content`], so the view
/// does no formatting. `remove` is the per-folder remove control, when the
/// dashboard is attached to an engine (R2.1), and `album` the per-folder album
/// mapping controls (R3.4).
fn status_card(
    id: ElementId,
    state_id: ElementId,
    connection_id: Option<ElementId>,
    card: &StatusCard,
    album: Option<AnyElement>,
    remove: Option<AnyElement>,
    cx: &App,
) -> impl IntoElement {
    let theme = cx.theme();
    let palette = CardPalette::new(theme);
    let content = card.content();
    let title = content.title.clone();
    let mut root = div()
        .id(id.clone())
        .test_support()
        .flex()
        .flex_col()
        .gap_3()
        .p_4()
        .w_full()
        .rounded(theme.radius_lg)
        .bg(palette.surface)
        .text_color(palette.text)
        .border_1()
        .border_color(theme.border)
        .child(
            div()
                .flex()
                .flex_row()
                .items_start()
                .justify_between()
                .gap_3()
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_0p5()
                        .min_w_0()
                        .child(
                            div()
                                .truncate()
                                .text_base()
                                .font_weight(FontWeight::MEDIUM)
                                .child(content.title),
                        )
                        .when_some(content.path, |this, path| {
                            this.child(
                                div()
                                    .truncate()
                                    .text_xs()
                                    .text_color(palette.meta)
                                    .child(path),
                            )
                        }),
                )
                .child(
                    div()
                        .id(state_id)
                        .test_support()
                        .aria_label(content.state.clone())
                        .child(state_tag(card.activity, content.state, &palette, theme)),
                ),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_3()
                .child(
                    Progress::new((id, "progress"))
                        .flex_1()
                        .value(content.progress * 100.0)
                        .accessibility_label(format!("{title} progress")),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(palette.meta)
                        .child(content.progress_percent),
                ),
        )
        .child(
            div()
                .text_sm()
                .text_color(palette.meta)
                .child(content.counts),
        )
        .when_some(content.album.clone(), |this, album| {
            this.child(div().text_sm().text_color(palette.meta).child(album))
        });

    if let Some(connection) = content.connection.clone()
        && let Some(id) = connection_id
    {
        root = root.child(
            div()
                .id(id)
                .test_support()
                .aria_label(connection.clone())
                .text_sm()
                .text_color(palette.meta)
                .child(connection),
        );
    }
    if let Some(review) = content.review {
        root = root.child(notice(review, theme.warning, &palette));
    }
    if let Some(error) = content.error {
        root = root.child(notice(
            format!("Last error: {error}"),
            theme.danger,
            &palette,
        ));
    }
    if let Some(album) = album {
        root = root.child(album);
    }
    if let Some(remove) = remove {
        root = root.child(div().flex().flex_row().justify_end().child(remove));
    }
    root
}

/// The activity-state pill on a card.
///
/// The wrapper owns the accessible name and the visible copy is inaccessible
/// to assistive technology, so the state is announced once, not twice.
fn state_tag(state: ActivityState, label: String, palette: &CardPalette, theme: &Theme) -> Tag {
    Tag::secondary()
        .bg(palette.pill_fill)
        .text_color(palette.pill_text)
        .border_color(state_border(state, theme))
        .child(Text::new_inaccessible(label.into()))
}

/// A card notice: a colored edge marks the kind, and the text keeps the
/// card's readable ink.
fn notice(text: String, accent: Hsla, palette: &CardPalette) -> impl IntoElement {
    div()
        .border_l_2()
        .border_color(accent)
        .pl_2()
        .text_xs()
        .text_color(palette.text)
        .child(text)
}
