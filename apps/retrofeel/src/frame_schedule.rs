//! App-wide ordering for work performed during Bevy's `Update` schedule.
//!
//! The emulator worker and ScreenCaptureKit callback remain RetroFeel's
//! capture clocks. These phases only describe how the Bevy shell ingests and
//! presents their results during one rendered app frame.

use bevy::prelude::*;

/// Stable frame vocabulary shared by RetroFeel's domain systems.
///
/// Systems in the same phase are intentionally unordered unless their local
/// registration declares a real data dependency. This lets Bevy run
/// independent work concurrently without hiding ordering in a catch-all
/// system chain.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum FramePhase {
    /// Read host input, operating-system requests, and completed attach work.
    Ingress,
    /// Exchange input, video, and status with the active game source.
    RunningSource,
    /// Apply completed background work and start newly requested jobs.
    JobCompletion,
    /// Translate host/UI input into application intent.
    UiIntent,
    /// Reconcile view state with the resulting application state.
    ViewMaintenance,
    /// Upload media and apply final visual/accessibility changes.
    Presentation,
}

pub(crate) struct FrameSchedulePlugin;

impl Plugin for FrameSchedulePlugin {
    fn build(&self, app: &mut App) {
        app.configure_sets(
            Update,
            (
                FramePhase::Ingress,
                FramePhase::RunningSource,
                FramePhase::JobCompletion,
                FramePhase::UiIntent,
                FramePhase::ViewMaintenance,
                FramePhase::Presentation,
            )
                .chain(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Resource, Default)]
    struct PhaseTrace(Vec<FramePhase>);

    fn mark_ingress(mut trace: ResMut<PhaseTrace>) {
        trace.0.push(FramePhase::Ingress);
    }

    fn mark_running_source(mut trace: ResMut<PhaseTrace>) {
        trace.0.push(FramePhase::RunningSource);
    }

    fn mark_job_completion(mut trace: ResMut<PhaseTrace>) {
        trace.0.push(FramePhase::JobCompletion);
    }

    fn mark_ui_intent(mut trace: ResMut<PhaseTrace>) {
        trace.0.push(FramePhase::UiIntent);
    }

    fn mark_view_maintenance(mut trace: ResMut<PhaseTrace>) {
        trace.0.push(FramePhase::ViewMaintenance);
    }

    fn mark_presentation(mut trace: ResMut<PhaseTrace>) {
        trace.0.push(FramePhase::Presentation);
    }

    #[test]
    fn frame_phases_run_in_data_flow_order() {
        let mut app = App::new();
        app.add_plugins(FrameSchedulePlugin)
            .init_resource::<PhaseTrace>()
            .add_systems(Update, mark_ingress.in_set(FramePhase::Ingress))
            .add_systems(
                Update,
                mark_running_source.in_set(FramePhase::RunningSource),
            )
            .add_systems(
                Update,
                mark_job_completion.in_set(FramePhase::JobCompletion),
            )
            .add_systems(Update, mark_ui_intent.in_set(FramePhase::UiIntent))
            .add_systems(
                Update,
                mark_view_maintenance.in_set(FramePhase::ViewMaintenance),
            )
            .add_systems(Update, mark_presentation.in_set(FramePhase::Presentation));

        app.update();

        assert_eq!(
            app.world().resource::<PhaseTrace>().0,
            [
                FramePhase::Ingress,
                FramePhase::RunningSource,
                FramePhase::JobCompletion,
                FramePhase::UiIntent,
                FramePhase::ViewMaintenance,
                FramePhase::Presentation,
            ]
        );
    }
}
