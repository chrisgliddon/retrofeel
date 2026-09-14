//! Non-activating local status overlay for externally captured Steam games.
//!
//! It is a separate transparent window, never part of `video.mkv`. The
//! ScreenCaptureKit selector also rejects every `RetroFeel*` title, including
//! this one, before it creates a single-window filter.

use std::collections::VecDeque;
use std::time::Instant;

use bevy::camera::{ClearColorConfig, RenderTarget};
use bevy::prelude::*;
use bevy::window::{CompositeAlphaMode, CursorOptions, WindowLevel, WindowRef};

use crate::plugin::{CoreResource, RecordingUiState};
use crate::screen::PendingFrame;

#[derive(Resource, Default)]
pub struct OverlayRequested(pub bool);

#[derive(Resource, Default)]
pub(crate) struct OverlayState {
    window: Option<Entity>,
    status_badge: Option<Entity>,
    status_text: Option<Entity>,
    timer_text: Option<Entity>,
    fps_text: Option<Entity>,
    mic_text: Option<Entity>,
    input_text: Option<Entity>,
    frame_times: VecDeque<Instant>,
    waveform: VecDeque<f32>,
}

/// Toggle the status window without adding any controls or input focus.
pub fn request_visible(commands: &mut Commands, visible: bool) {
    commands.insert_resource(OverlayRequested(visible));
}

pub fn sync_status_window(
    mut commands: Commands,
    requested: Res<OverlayRequested>,
    mut state: ResMut<OverlayState>,
    mut windows: Query<&mut Window>,
) {
    if state.window.is_none() && requested.0 {
        let (window_config, cursor_options) = status_window_config();
        let window = commands.spawn((window_config, cursor_options)).id();
        let camera = commands
            .spawn((
                Camera2d,
                Camera {
                    clear_color: ClearColorConfig::Custom(Color::NONE),
                    ..default()
                },
                RenderTarget::Window(WindowRef::Entity(window)),
            ))
            .id();
        let panel = commands
            .spawn((
                Node {
                    width: percent(100),
                    height: percent(100),
                    padding: UiRect::all(px(14)),
                    border: UiRect::all(px(1)),
                    border_radius: BorderRadius::all(px(12)),
                    flex_direction: FlexDirection::Column,
                    row_gap: px(9),
                    ..default()
                },
                BackgroundColor(Color::srgba(0.018, 0.027, 0.052, 0.94)),
                BorderColor::all(Color::srgba(0.31, 0.67, 0.90, 0.55)),
                UiTargetCamera(camera),
            ))
            .id();
        let header = commands
            .spawn((Node {
                width: percent(100),
                align_items: AlignItems::Center,
                justify_content: JustifyContent::SpaceBetween,
                column_gap: px(10),
                ..default()
            },))
            .id();
        let status_badge = commands
            .spawn((
                Node {
                    padding: UiRect {
                        left: px(9),
                        right: px(9),
                        top: px(4),
                        bottom: px(4),
                    },
                    border_radius: BorderRadius::MAX,
                    ..default()
                },
                BackgroundColor(Color::srgb(0.86, 0.18, 0.24)),
            ))
            .id();
        let status_text = spawn_label(
            &mut commands,
            "RECORDING",
            12.0,
            Color::srgb(1.0, 0.96, 0.96),
        );
        commands.entity(status_badge).add_child(status_text);
        let timer_text = spawn_label(&mut commands, "00:00", 22.0, Color::srgb(0.96, 0.98, 1.0));
        let fps_text = spawn_label(
            &mut commands,
            "CAPTURE --.- FPS",
            12.0,
            Color::srgb(0.49, 0.82, 1.0),
        );
        commands
            .entity(header)
            .add_children(&[status_badge, timer_text, fps_text]);

        let divider = commands
            .spawn((
                Node {
                    width: percent(100),
                    height: px(1),
                    ..default()
                },
                BackgroundColor(Color::srgba(0.35, 0.58, 0.78, 0.30)),
            ))
            .id();
        let mic_text = spawn_label(
            &mut commands,
            "MIC    ▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁",
            14.0,
            Color::srgb(0.51, 0.91, 0.82),
        );
        let input_text = spawn_label(
            &mut commands,
            "INPUT  idle",
            14.0,
            Color::srgb(0.90, 0.94, 1.0),
        );
        let footer = commands
            .spawn((
                Text::new("CLEAN MASTER  ·  GAME WINDOW ONLY"),
                TextFont {
                    font_size: 10.0,
                    ..default()
                },
                TextColor(Color::srgb(0.48, 0.61, 0.72)),
            ))
            .id();
        commands
            .entity(panel)
            .add_children(&[header, divider, mic_text, input_text, footer]);
        state.window = Some(window);
        state.status_badge = Some(status_badge);
        state.status_text = Some(status_text);
        state.timer_text = Some(timer_text);
        state.fps_text = Some(fps_text);
        state.mic_text = Some(mic_text);
        state.input_text = Some(input_text);
        return;
    }

    if let Some(window) = state.window {
        if let Ok(mut window) = windows.get_mut(window) {
            window.visible = requested.0;
        }
    }
}

fn status_window_config() -> (Window, CursorOptions) {
    (
        Window {
            title: "RetroFeel Status".into(),
            resolution: (440_u32, 164_u32).into(),
            transparent: true,
            composite_alpha_mode: CompositeAlphaMode::PostMultiplied,
            decorations: false,
            resizable: false,
            focused: false,
            window_level: WindowLevel::AlwaysOnTop,
            skip_taskbar: true,
            ..default()
        },
        CursorOptions {
            visible: false,
            hit_test: false,
            ..default()
        },
    )
}

/// Update the display only from live values that correspond to recording
/// inputs: newly delivered video, the ScreenCaptureKit held input snapshot,
/// and mic samples on their way to `mic.wav`.
pub fn update_status(
    requested: Res<OverlayRequested>,
    recording: Res<RecordingUiState>,
    core: Option<Res<CoreResource>>,
    pending: Res<PendingFrame>,
    mut state: ResMut<OverlayState>,
    mut labels: Query<&mut Text>,
    mut backgrounds: Query<&mut BackgroundColor>,
) {
    if !requested.0 {
        return;
    }
    let now = Instant::now();
    if pending.is_changed() && pending.frame.is_some() {
        state.frame_times.push_back(now);
    }
    while state
        .frame_times
        .front()
        .is_some_and(|frame_time| now.duration_since(*frame_time).as_secs_f32() > 3.0)
    {
        state.frame_times.pop_front();
    }
    let fps = match (state.frame_times.front(), state.frame_times.back()) {
        (Some(first), Some(last)) if first != last => {
            (state.frame_times.len().saturating_sub(1) as f32)
                / last.duration_since(*first).as_secs_f32()
        }
        _ => 0.0,
    };

    let (mic_peak, input_chips) = core
        .as_ref()
        .map(|core| {
            let input_chips = core
                .handle
                .input_slot()
                .lock()
                .map(|input| input_chips(&input.raw_host))
                .unwrap_or_else(|_| "input unavailable".into());
            (core.handle.mic_peak(), input_chips)
        })
        .unwrap_or((0.0, "no attached game".into()));
    if recording.active {
        state.waveform.push_back(mic_peak);
        while state.waveform.len() > 18 {
            state.waveform.pop_front();
        }
    } else {
        state.waveform.clear();
    }
    let waveform = if recording.active {
        waveform(&state.waveform)
    } else {
        "—".into()
    };
    let timer = recording
        .started_at
        .map(|started| format_duration(now.duration_since(started).as_secs()))
        .unwrap_or_else(|| "00:00".into());
    let state_label = if recording.active {
        "RECORDING"
    } else {
        "READY"
    };
    if let Some(entity) = state.status_badge {
        if let Ok(mut background) = backgrounds.get_mut(entity) {
            background.0 = if recording.active {
                Color::srgb(0.86, 0.18, 0.24)
            } else {
                Color::srgb(0.12, 0.46, 0.68)
            };
        }
    }
    set_label(&mut labels, state.status_text, state_label.into());
    set_label(&mut labels, state.timer_text, timer);
    set_label(
        &mut labels,
        state.fps_text,
        if fps > 0.0 {
            format!("CAPTURE {fps:4.1} FPS")
        } else {
            "CAPTURE --.- FPS".into()
        },
    );
    set_label(&mut labels, state.mic_text, format!("MIC    {waveform}"));
    set_label(
        &mut labels,
        state.input_text,
        format!("INPUT  {input_chips}"),
    );
}

fn spawn_label(commands: &mut Commands, text: &str, size: f32, color: Color) -> Entity {
    commands
        .spawn((
            Text::new(text),
            TextFont {
                font_size: size,
                ..default()
            },
            TextColor(color),
        ))
        .id()
}

fn set_label(labels: &mut Query<&mut Text>, entity: Option<Entity>, value: String) {
    let Some(entity) = entity else {
        return;
    };
    if let Ok(mut label) = labels.get_mut(entity) {
        **label = value;
    }
}

fn input_chips(raw: &retrofeel_types::RawHostInput) -> String {
    let mut chips = raw
        .keyboard_keys
        .iter()
        .take(3)
        .cloned()
        .collect::<Vec<_>>();
    chips.extend(raw.gamepad_buttons.iter().take(2).cloned());
    if raw.mouse.as_ref().is_some_and(|mouse| mouse.buttons != 0) {
        chips.push("Mouse".into());
    }
    if chips.is_empty() {
        "idle".into()
    } else {
        chips.join(" · ")
    }
}

fn waveform(samples: &VecDeque<f32>) -> String {
    const BARS: [&str; 8] = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
    const WIDTH: usize = 18;
    let visible = samples.len().min(WIDTH);
    let mut result = "▁".repeat(WIDTH - visible);
    let rendered = samples
        .iter()
        .skip(samples.len().saturating_sub(WIDTH))
        .map(|sample| {
            let index = (sample.clamp(0.0, 1.0) * (BARS.len() - 1) as f32).round() as usize;
            BARS[index]
        })
        .collect::<String>();
    result.push_str(&rendered);
    result
}

fn format_duration(seconds: u64) -> String {
    if seconds >= 3_600 {
        format!(
            "{:02}:{:02}:{:02}",
            seconds / 3_600,
            (seconds / 60) % 60,
            seconds % 60
        )
    } else {
        format!("{:02}:{:02}", seconds / 60, seconds % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waveform_uses_current_mic_samples() {
        let rendered = waveform(&VecDeque::from([0.0, 1.0]));
        assert_eq!(rendered.chars().count(), 18);
        assert!(rendered.ends_with("▁█"));
    }

    #[test]
    fn duration_is_compact_and_stable() {
        assert_eq!(format_duration(65), "01:05");
        assert_eq!(format_duration(3_661), "01:01:01");
    }

    #[test]
    fn status_window_is_non_activating_and_click_through() {
        let (window, cursor) = status_window_config();

        assert!(!window.focused);
        assert!(!window.decorations);
        assert!(!window.resizable);
        assert!(window.transparent);
        assert!(!cursor.visible);
        assert!(!cursor.hit_test);
    }
}
