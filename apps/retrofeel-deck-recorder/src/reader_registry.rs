use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

struct ActiveReader {
    event_path: PathBuf,
    generation: u64,
    cancel: Arc<AtomicBool>,
}

pub(crate) struct ReaderRegistration {
    pub(crate) generation: u64,
    pub(crate) cancel: Arc<AtomicBool>,
    pub(crate) replaced_path: Option<PathBuf>,
}

#[derive(Default)]
pub(crate) struct ReaderRegistry {
    active: HashMap<String, ActiveReader>,
    next_generation: u64,
}

impl ReaderRegistry {
    pub(crate) fn register(
        &mut self,
        device_id: &str,
        event_path: &Path,
    ) -> Option<ReaderRegistration> {
        let replaced_path = if let Some(active) = self.active.get(device_id) {
            if active.event_path == event_path {
                return None;
            }
            active.cancel.store(true, Ordering::Release);
            Some(active.event_path.clone())
        } else {
            None
        };

        self.next_generation = self
            .next_generation
            .checked_add(1)
            .expect("input reader generation should not overflow");
        let generation = self.next_generation;
        let cancel = Arc::new(AtomicBool::new(false));
        self.active.insert(
            device_id.to_string(),
            ActiveReader {
                event_path: event_path.to_path_buf(),
                generation,
                cancel: Arc::clone(&cancel),
            },
        );

        Some(ReaderRegistration {
            generation,
            cancel,
            replaced_path,
        })
    }

    pub(crate) fn finish(&mut self, device_id: &str, generation: u64) {
        if self
            .active
            .get(device_id)
            .is_some_and(|active| active.generation == generation)
        {
            self.active.remove(device_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::atomic::Ordering;

    use super::ReaderRegistry;

    #[test]
    fn replaces_a_logical_device_reader_when_its_event_path_changes() {
        let mut readers = ReaderRegistry::default();
        let first = readers
            .register("steam-pad-0", Path::new("/dev/input/event11"))
            .expect("first reader should start");

        assert!(
            readers
                .register("steam-pad-0", Path::new("/dev/input/event11"))
                .is_none(),
            "rediscovery at the same path must not start another reader"
        );

        let replacement = readers
            .register("steam-pad-0", Path::new("/dev/input/event22"))
            .expect("a new event path should replace the stale reader");
        assert_eq!(
            replacement.replaced_path.as_deref(),
            Some(Path::new("/dev/input/event11"))
        );
        assert!(first.cancel.load(Ordering::Acquire));
        assert!(!replacement.cancel.load(Ordering::Acquire));

        readers.finish("steam-pad-0", first.generation);
        assert!(
            readers
                .register("steam-pad-0", Path::new("/dev/input/event22"))
                .is_none(),
            "the stale reader finishing must not evict its replacement"
        );

        readers.finish("steam-pad-0", replacement.generation);
        assert!(
            readers
                .register("steam-pad-0", Path::new("/dev/input/event22"))
                .is_some(),
            "the active reader finishing should release the logical device"
        );
    }
}
