//! Framebuffer rendering: the core's RGBA8 frame → a Bevy `Image` asset,
//! displayed as a sprite with aspect-correct integer scaling.

use bevy::asset::RenderAssetUsages;
use bevy::image::{Image, ImageSampler};
use bevy::prelude::*;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};
use retrofeel_types::VideoConfig;

#[derive(Resource, Clone)]
pub struct ScreenSettings {
    pub integer_scaling: bool,
    pub aspect_correction: bool,
}

impl From<&VideoConfig> for ScreenSettings {
    fn from(value: &VideoConfig) -> Self {
        Self {
            integer_scaling: value.integer_scaling,
            aspect_correction: value.aspect_correction,
        }
    }
}

/// Marker for the sprite that displays the core framebuffer.
#[derive(Component)]
pub struct CoreScreen;

/// Marker for the texture asset handle.
#[derive(Component)]
pub struct CoreTexture(pub Handle<Image>);

/// The latest frame received from the core thread, waiting to be uploaded.
#[derive(Resource, Default)]
pub struct PendingFrame {
    pub frame: Option<std::sync::Arc<crate::core_thread::CoreFrame>>,
    pub width: u32,
    pub height: u32,
}

/// Spawn the camera used by both UI and gameplay.
pub fn setup_camera(commands: &mut Commands) {
    commands.spawn(Camera2d);
}

/// Spawn the core screen sprite with an initial 256×224 texture.
pub fn setup_screen(commands: &mut Commands, images: &mut Assets<Image>) {
    let (w, h) = (256u32, 224u32);
    let mut image = Image::new_fill(
        Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        &[0, 0, 0, 255],
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::default(),
    );
    image.sampler = ImageSampler::nearest();
    let handle = images.add(image);

    commands.spawn((
        Sprite::from_image(handle.clone()),
        Transform::from_xyz(0.0, 0.0, 0.0),
        CoreScreen,
        CoreTexture(handle),
    ));
}

/// Upload the latest core frame into the texture asset (in-place, no realloc
/// unless the core's resolution changed).
pub fn upload_frame(
    texture: Query<&CoreTexture, With<CoreScreen>>,
    pending: Res<PendingFrame>,
    settings: Option<Res<ScreenSettings>>,
    mut images: ResMut<Assets<Image>>,
    mut screen_transform: Query<&mut Transform, With<CoreScreen>>,
    window: Query<&Window>,
) {
    let Some(cf) = pending.frame.as_ref() else {
        return;
    };
    let Some(frame) = cf.frame.as_ref() else {
        return;
    };
    let Ok(texture) = texture.single() else {
        return;
    };
    let Ok(mut screen_transform) = screen_transform.single_mut() else {
        return;
    };
    let Ok(window) = window.single() else {
        return;
    };
    let handle = &texture.0;
    let img = match images.get_mut(handle) {
        Some(i) => i,
        None => return,
    };

    // Resize the texture if the core's resolution changed.
    let cur_w = img.width();
    let cur_h = img.height();
    if cur_w != frame.width || cur_h != frame.height {
        img.resize(Extent3d {
            width: frame.width,
            height: frame.height,
            depth_or_array_layers: 1,
        });
        img.sampler = ImageSampler::nearest();
    }

    // In-place copy. `img.data` is `Option<Vec<u8>>`.
    if let Some(data) = img.data.as_mut() {
        let len = (frame.width as usize) * (frame.height as usize) * 4;
        if data.len() >= len {
            data[..len].copy_from_slice(&frame.rgba);
        }
    }

    // Aspect-correct scaling, optionally snapped to an integer multiplier.
    let (ww, wh) = (window.width(), window.height());
    let (iw, ih) = (frame.width as f32, frame.height as f32);
    if iw > 0.0 && ih > 0.0 && ww > 0.0 && wh > 0.0 {
        let settings = settings.as_deref();
        if settings
            .map(|settings| settings.aspect_correction)
            .unwrap_or(true)
        {
            let mut scale = (ww / iw).min(wh / ih);
            if settings
                .map(|settings| settings.integer_scaling)
                .unwrap_or(true)
            {
                scale = scale.floor().max(1.0);
            }
            screen_transform.scale = Vec3::splat(scale);
        } else {
            screen_transform.scale = Vec3::new(ww / iw, wh / ih, 1.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn add_core_screen(mut commands: Commands, mut images: ResMut<Assets<Image>>) {
        setup_screen(&mut commands, &mut images);
    }

    #[test]
    fn gameplay_screen_reuses_the_existing_window() {
        let mut app = App::new();
        app.init_resource::<Assets<Image>>()
            .add_systems(Update, add_core_screen);
        app.world_mut().spawn(Window::default());

        app.update();

        let mut windows = app.world_mut().query::<&Window>();
        assert_eq!(windows.iter(app.world()).count(), 1);
        let mut screens = app.world_mut().query::<&CoreScreen>();
        assert_eq!(screens.iter(app.world()).count(), 1);
    }
}
