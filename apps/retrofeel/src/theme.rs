//! Semantic application theming and embedded typography.
//!
//! The app chrome follows the persisted appearance preference. Gameplay HUD
//! surfaces deliberately use [`INGAME`] instead so the game remains legible
//! regardless of the desktop theme.

use std::sync::atomic::{AtomicU8, Ordering};

use bevy::prelude::*;
use bevy::window::{PrimaryWindow, WindowTheme, WindowThemeChanged};
use retrofeel_types::ThemePreference;

use crate::plugin::AppState;
use crate::ui::FrontendModel;

type ChangedTextFonts<'w, 's> =
    Query<'w, 's, (Entity, &'static mut TextFont), Or<(Added<TextFont>, Changed<TextFont>)>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ResolvedTheme {
    Light,
    #[default]
    Dark,
}

impl ResolvedTheme {
    const fn as_u8(self) -> u8 {
        match self {
            Self::Light => 0,
            Self::Dark => 1,
        }
    }

    const fn from_u8(value: u8) -> Self {
        if value == 0 {
            Self::Light
        } else {
            Self::Dark
        }
    }
}

static CURRENT_THEME: AtomicU8 = AtomicU8::new(ResolvedTheme::Dark.as_u8());

pub fn current_theme() -> ResolvedTheme {
    ResolvedTheme::from_u8(CURRENT_THEME.load(Ordering::Relaxed))
}

fn set_current_theme(theme: ResolvedTheme) {
    CURRENT_THEME.store(theme.as_u8(), Ordering::Relaxed);
}

#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub app_background: Color,
    pub surface: Color,
    pub raised_surface: Color,
    pub primary_text: Color,
    pub muted_text: Color,
    pub subtle_border: Color,
    pub control_border: Color,
    pub selected_surface: Color,
    pub focus_ring: Color,
    pub action: Color,
    pub action_hover: Color,
    pub action_pressed: Color,
    pub action_text: Color,
    pub favorite: Color,
    pub success_fill: Color,
    pub success_text: Color,
    pub warning_text: Color,
    pub error_text: Color,
    pub info_text: Color,
}

pub const LIGHT: Palette = Palette {
    app_background: Color::srgb_u8(0xF8, 0xF7, 0xF4),
    surface: Color::srgb_u8(0xFF, 0xFF, 0xFF),
    raised_surface: Color::srgb_u8(0xEF, 0xED, 0xE6),
    primary_text: Color::srgb_u8(0x0A, 0x16, 0x28),
    muted_text: Color::srgb_u8(0x5A, 0x64, 0x73),
    subtle_border: Color::srgb_u8(0xE6, 0xE8, 0xEB),
    control_border: Color::srgb_u8(0x75, 0x7E, 0x8B),
    selected_surface: Color::srgb_u8(0xE5, 0xF6, 0xFC),
    focus_ring: Color::srgb_u8(0x00, 0x7A, 0x9E),
    action: Color::srgb_u8(0x00, 0xAC, 0xE0),
    action_hover: Color::srgb_u8(0x00, 0x9F, 0xCF),
    action_pressed: Color::srgb_u8(0x00, 0x90, 0xBE),
    action_text: Color::srgb_u8(0x0A, 0x16, 0x28),
    favorite: Color::srgb_u8(0x8A, 0x68, 0x00),
    success_fill: Color::srgb_u8(0x14, 0xB8, 0xA6),
    success_text: Color::srgb_u8(0x0E, 0x6F, 0x66),
    warning_text: Color::srgb_u8(0x9A, 0x4C, 0x16),
    error_text: Color::srgb_u8(0xB1, 0x11, 0x38),
    info_text: Color::srgb_u8(0x00, 0x67, 0x85),
};

pub const DARK: Palette = Palette {
    app_background: Color::srgb_u8(0x0A, 0x16, 0x28),
    surface: Color::srgb_u8(0x12, 0x1F, 0x32),
    raised_surface: Color::srgb_u8(0x1A, 0x2A, 0x40),
    primary_text: Color::srgb_u8(0xF8, 0xF7, 0xF4),
    muted_text: Color::srgb_u8(0xAE, 0xB9, 0xC8),
    subtle_border: Color::srgb_u8(0x2C, 0x3D, 0x55),
    control_border: Color::srgb_u8(0x70, 0x87, 0xA6),
    selected_surface: Color::srgb_u8(0x12, 0x3B, 0x4A),
    focus_ring: Color::srgb_u8(0x00, 0xAC, 0xE0),
    action: Color::srgb_u8(0x00, 0xAC, 0xE0),
    action_hover: Color::srgb_u8(0x00, 0x9F, 0xCF),
    action_pressed: Color::srgb_u8(0x00, 0x90, 0xBE),
    action_text: Color::srgb_u8(0x0A, 0x16, 0x28),
    favorite: Color::srgb_u8(0xFF, 0xE0, 0x00),
    success_fill: Color::srgb_u8(0x14, 0xB8, 0xA6),
    success_text: Color::srgb_u8(0x4D, 0xD8, 0xC7),
    warning_text: Color::srgb_u8(0xFF, 0xD9, 0x5C),
    error_text: Color::srgb_u8(0xFF, 0x78, 0x95),
    info_text: Color::srgb_u8(0x5E, 0xD4, 0xF1),
};

pub const fn palette(theme: ResolvedTheme) -> &'static Palette {
    match theme {
        ResolvedTheme::Light => &LIGHT,
        ResolvedTheme::Dark => &DARK,
    }
}

#[derive(Debug, Clone, Copy)]
pub struct LayoutTokens {
    pub space_1: f32,
    pub space_2: f32,
    pub space_4: f32,
    pub space_8: f32,
    pub control_radius: f32,
    pub card_radius: f32,
    pub modal_radius: f32,
}

pub const LAYOUT: LayoutTokens = LayoutTokens {
    space_1: 4.0,
    space_2: 8.0,
    space_4: 16.0,
    space_8: 32.0,
    control_radius: 8.0,
    card_radius: 12.0,
    modal_radius: 16.0,
};

#[derive(Debug, Clone, Copy)]
pub struct InGamePalette {
    pub backdrop: Color,
    pub panel: Color,
    pub text: Color,
    pub muted: Color,
    pub action: Color,
    pub destructive: Color,
    pub border: Color,
}

pub const INGAME: InGamePalette = InGamePalette {
    backdrop: Color::srgba(0.01, 0.04, 0.08, 0.78),
    panel: Color::srgba(0.04, 0.09, 0.16, 0.95),
    text: Color::srgb_u8(0xF8, 0xF7, 0xF4),
    muted: Color::srgb_u8(0xAE, 0xB9, 0xC8),
    action: Color::srgb_u8(0x00, 0xAC, 0xE0),
    destructive: Color::srgb_u8(0xFF, 0x4D, 0x6D),
    border: Color::srgb_u8(0x5B, 0x72, 0x91),
};

pub fn quiet_card_shadow() -> BoxShadow {
    match current_theme() {
        ResolvedTheme::Light => BoxShadow::new(
            Color::srgba(0.04, 0.09, 0.16, 0.08),
            px(0.0),
            px(1.0),
            px(0.0),
            px(2.0),
        ),
        ResolvedTheme::Dark => BoxShadow::default(),
    }
}

#[derive(Resource, Debug, Clone, Copy)]
pub struct ThemeRuntime {
    pub preference: ThemePreference,
    pub resolved: ResolvedTheme,
    pub system_theme: Option<ResolvedTheme>,
}

impl Default for ThemeRuntime {
    fn default() -> Self {
        Self {
            preference: ThemePreference::System,
            resolved: ResolvedTheme::Dark,
            system_theme: None,
        }
    }
}

pub const fn resolve_theme(
    preference: ThemePreference,
    system_theme: Option<ResolvedTheme>,
) -> ResolvedTheme {
    match preference {
        ThemePreference::System => match system_theme {
            Some(theme) => theme,
            None => ResolvedTheme::Dark,
        },
        ThemePreference::Light => ResolvedTheme::Light,
        ThemePreference::Dark => ResolvedTheme::Dark,
    }
}

const fn from_window_theme(theme: WindowTheme) -> ResolvedTheme {
    match theme {
        WindowTheme::Light => ResolvedTheme::Light,
        WindowTheme::Dark => ResolvedTheme::Dark,
    }
}

pub fn initialize_theme(
    model: Res<FrontendModel>,
    mut runtime: ResMut<ThemeRuntime>,
    mut windows: Query<&mut Window, With<PrimaryWindow>>,
    mut clear: ResMut<ClearColor>,
) {
    let system_theme = windows
        .single()
        .ok()
        .and_then(|window| window.window_theme)
        .map(from_window_theme);
    runtime.preference = model.config.appearance.theme;
    runtime.system_theme = system_theme;
    runtime.resolved = resolve_theme(runtime.preference, runtime.system_theme);
    set_current_theme(runtime.resolved);
    clear.0 = palette(runtime.resolved).app_background;
    if let Ok(mut window) = windows.single_mut() {
        window.window_theme = match runtime.preference {
            ThemePreference::System => None,
            ThemePreference::Light => Some(WindowTheme::Light),
            ThemePreference::Dark => Some(WindowTheme::Dark),
        };
    }
}

pub fn sync_theme(
    model: Res<FrontendModel>,
    state: Res<State<AppState>>,
    mut next: ResMut<NextState<AppState>>,
    mut runtime: ResMut<ThemeRuntime>,
    mut events: MessageReader<WindowThemeChanged>,
    mut windows: Query<(Entity, &mut Window), With<PrimaryWindow>>,
    mut clear: ResMut<ClearColor>,
) {
    let Ok((window_entity, mut window)) = windows.single_mut() else {
        return;
    };

    if runtime.preference == ThemePreference::System {
        for event in events.read() {
            if event.window == window_entity {
                runtime.system_theme = Some(from_window_theme(event.theme));
            }
        }
    } else {
        events.clear();
    }

    let preference = model.config.appearance.theme;
    if preference != runtime.preference {
        runtime.preference = preference;
        window.window_theme = match preference {
            ThemePreference::System => None,
            ThemePreference::Light => Some(WindowTheme::Light),
            ThemePreference::Dark => Some(WindowTheme::Dark),
        };
    }

    let resolved = resolve_theme(runtime.preference, runtime.system_theme);
    if resolved == runtime.resolved {
        return;
    }

    runtime.resolved = resolved;
    set_current_theme(resolved);
    clear.0 = palette(resolved).app_background;
    if matches!(
        state.get(),
        AppState::FirstRun | AppState::Library | AppState::Settings
    ) {
        next.set(state.get().clone());
    }
}

#[derive(Resource, Clone)]
pub struct UiFonts {
    pub regular: Handle<Font>,
    pub medium: Handle<Font>,
    pub heading: Handle<Font>,
}

const BARLOW_REGULAR: &[u8] = include_bytes!("../assets/fonts/Barlow-Regular.ttf");
const BARLOW_MEDIUM: &[u8] = include_bytes!("../assets/fonts/Barlow-Medium.ttf");
const BARLOW_HEADING: &[u8] = include_bytes!("../assets/fonts/BarlowCondensed-SemiBold.ttf");

pub fn install_embedded_fonts(mut commands: Commands, mut assets: ResMut<Assets<Font>>) {
    let default_handle = TextFont::default().font;
    let Ok(regular) = Font::try_from_bytes(BARLOW_REGULAR.to_vec()) else {
        log::error!("embedded Barlow Regular font is invalid");
        return;
    };
    let Ok(medium) = Font::try_from_bytes(BARLOW_MEDIUM.to_vec()) else {
        log::error!("embedded Barlow Medium font is invalid");
        return;
    };
    let Ok(heading) = Font::try_from_bytes(BARLOW_HEADING.to_vec()) else {
        log::error!("embedded Barlow Condensed SemiBold font is invalid");
        return;
    };
    if assets.insert(&default_handle, regular).is_err() {
        log::error!("could not replace Bevy's default font with embedded Barlow Regular");
        return;
    }
    commands.insert_resource(UiFonts {
        regular: default_handle,
        medium: assets.add(medium),
        heading: assets.add(heading),
    });
}

pub fn apply_typography(
    fonts: Option<Res<UiFonts>>,
    buttons: Query<(), With<crate::ui::UiButton>>,
    parents: Query<&ChildOf>,
    mut text: ChangedTextFonts,
) {
    let Some(fonts) = fonts else {
        return;
    };
    for (entity, mut text_font) in &mut text {
        // Secondary labels remain readable at the default desktop UI scale.
        if text_font.font_size < 14.0 {
            text_font.font_size = 14.0;
        }
        let in_button = parents
            .get(entity)
            .ok()
            .is_some_and(|parent| buttons.contains(parent.parent()));
        let desired = if text_font.font_size >= 20.0 {
            &fonts.heading
        } else if in_button {
            &fonts.medium
        } else {
            &fonts.regular
        };
        if &text_font.font != desired {
            text_font.font = desired.clone();
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ButtonVariant {
    Primary,
    #[default]
    Secondary,
    Danger,
    Nav,
    Segment,
    Icon,
    InGame,
    InGameDanger,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ButtonVisual {
    pub background: Color,
    pub border: Color,
    pub foreground: Color,
}

pub fn button_visual(
    theme: ResolvedTheme,
    variant: ButtonVariant,
    interaction: Interaction,
    selected: bool,
) -> ButtonVisual {
    let colors = palette(theme);
    if matches!(variant, ButtonVariant::InGame | ButtonVariant::InGameDanger) {
        let danger = variant == ButtonVariant::InGameDanger;
        return ButtonVisual {
            background: match interaction {
                Interaction::Pressed if danger => Color::srgba(0.55, 0.03, 0.12, 0.98),
                Interaction::Hovered if danger => Color::srgba(0.35, 0.03, 0.09, 0.98),
                Interaction::Pressed => Color::srgba(0.0, 0.56, 0.75, 0.98),
                Interaction::Hovered => Color::srgba(0.0, 0.48, 0.64, 0.98),
                Interaction::None => Color::srgba(0.04, 0.09, 0.16, 0.96),
            },
            border: if interaction == Interaction::None {
                INGAME.border
            } else if danger {
                INGAME.destructive
            } else {
                INGAME.action
            },
            foreground: if danger && interaction == Interaction::None {
                INGAME.destructive
            } else {
                INGAME.text
            },
        };
    }

    let active = interaction != Interaction::None;
    match variant {
        ButtonVariant::Primary => ButtonVisual {
            background: match interaction {
                Interaction::Pressed => colors.action_pressed,
                Interaction::Hovered => colors.action_hover,
                Interaction::None => colors.action,
            },
            border: match interaction {
                Interaction::Pressed => colors.action_pressed,
                Interaction::Hovered => colors.action_hover,
                Interaction::None => colors.action,
            },
            foreground: colors.action_text,
        },
        ButtonVariant::Danger => ButtonVisual {
            background: if active {
                colors.error_text
            } else {
                Color::NONE
            },
            border: colors.error_text,
            foreground: if active {
                colors.surface
            } else {
                colors.error_text
            },
        },
        ButtonVariant::Nav | ButtonVariant::Segment => ButtonVisual {
            background: if selected || active {
                colors.selected_surface
            } else {
                Color::NONE
            },
            border: if interaction == Interaction::None {
                Color::NONE
            } else {
                colors.focus_ring
            },
            foreground: colors.primary_text,
        },
        ButtonVariant::Secondary | ButtonVariant::Icon => ButtonVisual {
            background: if selected || interaction == Interaction::Pressed {
                colors.selected_surface
            } else if interaction == Interaction::Hovered {
                colors.raised_surface
            } else {
                colors.surface
            },
            border: if active {
                colors.focus_ring
            } else {
                colors.control_border
            },
            foreground: colors.primary_text,
        },
        ButtonVariant::InGame | ButtonVariant::InGameDanger => unreachable!(),
    }
}

pub fn current_button_visual(
    variant: ButtonVariant,
    interaction: Interaction,
    selected: bool,
) -> ButtonVisual {
    button_visual(current_theme(), variant, interaction, selected)
}

// Unique dark-era marker values retained only as a migration bridge for the
// existing screen builders. `apply_shell_theme_colors` resolves each marker
// to a semantic token before the frame is rendered. New builders should use
// `palette(current_theme())` or `current_button_visual` directly.
pub const PALETTE_BG: Color = Color::srgb(0.055, 0.058, 0.063);
pub const PALETTE_TOOLBAR: Color = Color::srgb(0.125, 0.129, 0.137);
pub const PALETTE_SIDE: Color = Color::srgb(0.105, 0.112, 0.122);
pub const PALETTE_PANEL: Color = Color::srgb(0.145, 0.151, 0.161);
pub const PALETTE_PREF_WINDOW: Color = Color::srgb(0.115, 0.12, 0.128);
pub const PALETTE_PREF_TABS: Color = Color::srgb(0.132, 0.138, 0.148);
pub const PALETTE_PREF_CARD: Color = Color::srgb(0.118, 0.126, 0.136);
pub const PALETTE_PREF_TAB_ACTIVE: Color = Color::srgb(0.19, 0.32, 0.53);
pub const PALETTE_BUTTON: Color = Color::srgb(0.17, 0.18, 0.195);
pub const PALETTE_TOOL_BUTTON: Color = Color::srgb(0.155, 0.162, 0.174);
pub const PALETTE_BUTTON_HOVER: Color = Color::srgb(0.22, 0.235, 0.255);
pub const PALETTE_BUTTON_ACTIVE: Color = Color::srgb(0.00, 0.36, 0.78);
pub const PALETTE_SEGMENT_BG: Color = Color::srgb(0.09, 0.095, 0.105);
pub const PALETTE_SEARCH: Color = Color::srgb(0.08, 0.085, 0.094);
pub const PALETTE_TILE: Color = Color::srgb(0.118, 0.126, 0.137);
pub const PALETTE_ROW: Color = Color::srgb(0.13, 0.138, 0.148);
pub const PALETTE_EMPTY: Color = Color::srgb(0.115, 0.121, 0.13);
pub const PALETTE_LINE: Color = Color::srgb(0.31, 0.33, 0.36);
pub const PALETTE_LINE_DARK: Color = Color::srgb(0.075, 0.08, 0.088);
pub const PALETTE_POSTER_BORDER: Color = Color::srgb(0.05, 0.052, 0.058);
pub const PALETTE_TEXT: Color = Color::srgb(0.91, 0.92, 0.90);
pub const PALETTE_MUTED: Color = Color::srgb(0.62, 0.66, 0.68);
pub const PALETTE_MUTED_DARK: Color = Color::srgb(0.33, 0.36, 0.38);
pub const PALETTE_ACCENT: Color = Color::srgb(0.32, 0.62, 0.96);
pub const PALETTE_STAR: Color = Color::srgb(0.96, 0.73, 0.28);
pub const PALETTE_GOOD: Color = Color::srgb(0.52, 0.82, 0.48);
pub const PALETTE_WARN: Color = Color::srgb(0.91, 0.74, 0.38);
pub const PALETTE_BAD: Color = Color::srgb(0.91, 0.42, 0.40);
pub const PALETTE_INFO: Color = Color::srgb(0.55, 0.70, 0.88);

fn resolve_marker(color: Color, colors: &Palette) -> Option<Color> {
    Some(if color == PALETTE_BG {
        colors.app_background
    } else if matches!(color, PALETTE_TOOLBAR | PALETTE_SIDE | PALETTE_PREF_WINDOW) {
        colors.surface
    } else if color == PALETTE_PANEL {
        colors.app_background
    } else if matches!(color, PALETTE_PREF_TABS | PALETTE_SEGMENT_BG) {
        colors.raised_surface
    } else if matches!(
        color,
        PALETTE_PREF_CARD | PALETTE_TILE | PALETTE_ROW | PALETTE_BUTTON | PALETTE_TOOL_BUTTON
    ) {
        colors.surface
    } else if matches!(
        color,
        PALETTE_PREF_TAB_ACTIVE | PALETTE_BUTTON_HOVER | PALETTE_EMPTY | PALETTE_SEARCH
    ) {
        colors.selected_surface
    } else if color == PALETTE_BUTTON_ACTIVE {
        colors.action
    } else if color == PALETTE_LINE {
        colors.control_border
    } else if matches!(color, PALETTE_LINE_DARK | PALETTE_POSTER_BORDER) {
        colors.subtle_border
    } else if color == PALETTE_TEXT {
        colors.primary_text
    } else if matches!(color, PALETTE_MUTED | PALETTE_MUTED_DARK) {
        colors.muted_text
    } else if color == PALETTE_ACCENT {
        colors.focus_ring
    } else if color == PALETTE_STAR {
        colors.favorite
    } else if color == PALETTE_GOOD {
        colors.success_text
    } else if color == PALETTE_WARN {
        colors.warning_text
    } else if color == PALETTE_BAD {
        colors.error_text
    } else if color == PALETTE_INFO {
        colors.info_text
    } else {
        return None;
    })
}

pub fn apply_shell_theme_colors(
    state: Res<State<AppState>>,
    runtime: Res<ThemeRuntime>,
    mut backgrounds: Query<&mut BackgroundColor, Changed<BackgroundColor>>,
    mut borders: Query<&mut BorderColor, Changed<BorderColor>>,
    mut text: Query<&mut TextColor, Changed<TextColor>>,
    mut images: Query<&mut ImageNode, Changed<ImageNode>>,
) {
    if matches!(state.get(), AppState::InGame | AppState::InGameOverlay) {
        return;
    }
    let colors = palette(runtime.resolved);
    let _success_fill_token = colors.success_fill;
    for mut background in &mut backgrounds {
        if let Some(color) = resolve_marker(background.0, colors) {
            background.0 = color;
        }
    }
    for mut border in &mut borders {
        if let Some(color) = resolve_marker(border.top, colors) {
            *border = BorderColor::all(color);
        }
    }
    for mut text in &mut text {
        if let Some(color) = resolve_marker(text.0, colors) {
            text.0 = color;
        }
    }
    for mut image in &mut images {
        if let Some(color) = resolve_marker(image.color, colors) {
            image.color = color;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn srgb(color: Color) -> [f32; 3] {
        color.to_srgba().to_f32_array()[..3].try_into().unwrap()
    }

    fn linear(channel: f32) -> f32 {
        if channel <= 0.04045 {
            channel / 12.92
        } else {
            ((channel + 0.055) / 1.055).powf(2.4)
        }
    }

    fn contrast(a: Color, b: Color) -> f32 {
        let luminance = |color: Color| {
            let [r, g, b] = srgb(color);
            0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b)
        };
        let (bright, dark) = {
            let a = luminance(a);
            let b = luminance(b);
            if a >= b {
                (a, b)
            } else {
                (b, a)
            }
        };
        (bright + 0.05) / (dark + 0.05)
    }

    #[test]
    fn resolves_explicit_and_system_themes() {
        assert_eq!(
            resolve_theme(ThemePreference::System, Some(ResolvedTheme::Light)),
            ResolvedTheme::Light
        );
        assert_eq!(
            resolve_theme(ThemePreference::System, Some(ResolvedTheme::Dark)),
            ResolvedTheme::Dark
        );
        assert_eq!(
            resolve_theme(ThemePreference::System, None),
            ResolvedTheme::Dark
        );
        assert_eq!(
            resolve_theme(ThemePreference::Light, Some(ResolvedTheme::Dark)),
            ResolvedTheme::Light
        );
        assert_eq!(
            resolve_theme(ThemePreference::Dark, Some(ResolvedTheme::Light)),
            ResolvedTheme::Dark
        );
    }

    #[test]
    fn text_and_actions_meet_contrast_targets() {
        for colors in [LIGHT, DARK] {
            for background in [
                colors.app_background,
                colors.surface,
                colors.raised_surface,
                colors.selected_surface,
            ] {
                for text in [
                    colors.primary_text,
                    colors.muted_text,
                    colors.success_text,
                    colors.warning_text,
                    colors.error_text,
                    colors.info_text,
                ] {
                    assert!(contrast(text, background) >= 4.5);
                }
                for indicator in [colors.focus_ring, colors.control_border, colors.favorite] {
                    assert!(contrast(indicator, background) >= 3.0);
                }
            }
            for background in [colors.action, colors.action_hover, colors.action_pressed] {
                assert!(contrast(colors.action_text, background) >= 4.5);
            }
        }
    }

    #[test]
    fn typography_keeps_secondary_labels_readable_after_updates() {
        let mut app = App::new();
        app.insert_resource(UiFonts {
            regular: Handle::default(),
            medium: Handle::default(),
            heading: Handle::default(),
        });
        app.add_systems(Update, apply_typography);
        let label = app
            .world_mut()
            .spawn(TextFont {
                font_size: 11.0,
                ..default()
            })
            .id();
        app.update();
        assert_eq!(app.world().get::<TextFont>(label).unwrap().font_size, 14.0);
        app.world_mut()
            .get_mut::<TextFont>(label)
            .unwrap()
            .font_size = 24.0;
        app.update();
        assert_eq!(app.world().get::<TextFont>(label).unwrap().font_size, 24.0);
        app.world_mut()
            .get_mut::<TextFont>(label)
            .unwrap()
            .font_size = 10.0;
        app.update();
        assert_eq!(app.world().get::<TextFont>(label).unwrap().font_size, 14.0);
    }

    #[test]
    fn selected_button_stays_selected_at_rest() {
        for theme in [ResolvedTheme::Light, ResolvedTheme::Dark] {
            let visual = button_visual(theme, ButtonVariant::Nav, Interaction::None, true);
            assert_eq!(visual.background, palette(theme).selected_surface);
            assert_eq!(visual.foreground, palette(theme).primary_text);
        }
    }

    #[test]
    fn primary_button_states_use_shared_sky_tokens() {
        for theme in [ResolvedTheme::Light, ResolvedTheme::Dark] {
            let colors = palette(theme);
            assert_eq!(
                button_visual(theme, ButtonVariant::Primary, Interaction::None, false).background,
                colors.action
            );
            assert_eq!(
                button_visual(theme, ButtonVariant::Primary, Interaction::Hovered, false)
                    .background,
                colors.action_hover
            );
            assert_eq!(
                button_visual(theme, ButtonVariant::Primary, Interaction::Pressed, false)
                    .background,
                colors.action_pressed
            );
        }
    }
}
