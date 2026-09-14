use std::collections::BTreeMap;

use libretro_host::{Core, CoreVariable};
use retrofeel_types::RetroFeelConfig;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCoreOption {
    pub key: String,
    pub description: String,
    pub default_value: Option<String>,
    pub values: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CoreOptionApplyReport {
    pub core_key: String,
    pub available: Vec<ParsedCoreOption>,
    pub applied: BTreeMap<String, String>,
    pub unknown: BTreeMap<String, String>,
    pub invalid: Vec<InvalidCoreOption>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidCoreOption {
    pub key: String,
    pub value: String,
    pub allowed_values: Vec<String>,
}

pub fn parse_core_variable(variable: &CoreVariable) -> ParsedCoreOption {
    let (description, values_text) = variable
        .option_text
        .split_once(';')
        .map(|(desc, values)| (desc.trim(), values.trim()))
        .unwrap_or(("", variable.option_text.trim()));

    let values: Vec<String> = values_text
        .split('|')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect();

    ParsedCoreOption {
        key: variable.key.clone(),
        description: description.to_string(),
        default_value: values.first().cloned(),
        values,
    }
}

pub fn apply_configured_core_options(
    core: &Core,
    config: &RetroFeelConfig,
) -> CoreOptionApplyReport {
    let core_key = core.system_info().library_name.clone();
    apply_core_options(core, &core_key, config.core_options_for(&core_key))
}

pub fn apply_core_options(
    core: &Core,
    core_key: &str,
    overrides: BTreeMap<String, String>,
) -> CoreOptionApplyReport {
    let available: Vec<ParsedCoreOption> =
        core.variables().iter().map(parse_core_variable).collect();
    let mut by_key: BTreeMap<String, ParsedCoreOption> = BTreeMap::new();
    for option in &available {
        by_key.insert(option.key.clone(), option.clone());
    }

    let mut report = CoreOptionApplyReport {
        core_key: core_key.to_string(),
        available,
        ..Default::default()
    };

    for (key, value) in overrides {
        let Some(option) = by_key.get(&key) else {
            report.unknown.insert(key, value);
            continue;
        };
        if !option.values.is_empty() && !option.values.iter().any(|allowed| allowed == &value) {
            report.invalid.push(InvalidCoreOption {
                key,
                value,
                allowed_values: option.values.clone(),
            });
            continue;
        }

        core.set_option(&key, &value);
        report.applied.insert(key, value);
    }

    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use libretro_host::Core;
    use std::path::PathBuf;

    #[test]
    fn parses_retro_variable_text() {
        let parsed = parse_core_variable(&CoreVariable {
            key: "palette".into(),
            option_text: "Palette; blue|green|red".into(),
        });

        assert_eq!(parsed.description, "Palette");
        assert_eq!(parsed.default_value.as_deref(), Some("blue"));
        assert_eq!(parsed.values, vec!["blue", "green", "red"]);
    }

    fn mock_core_path() -> PathBuf {
        PathBuf::from(env!("MOCK_CORE_PATH"))
    }

    #[test]
    fn applies_valid_core_option_before_load_game() {
        let mut core = Core::load(mock_core_path(), "system").unwrap();
        let mut overrides = BTreeMap::new();
        overrides.insert("mock_palette".into(), "green".into());

        let report = apply_core_options(&core, "mock-core", overrides);

        assert_eq!(
            report.applied.get("mock_palette").map(String::as_str),
            Some("green")
        );
        assert!(report.invalid.is_empty());
        assert!(report.unknown.is_empty());

        core.load_game(&[], None).unwrap();
        let frame = core.run_frame_required(Default::default()).unwrap();
        assert_eq!(frame.rgba[1], 0xEE);
    }

    #[test]
    fn reports_invalid_and_unknown_core_options() {
        let core = Core::load(mock_core_path(), "system").unwrap();
        let mut overrides = BTreeMap::new();
        overrides.insert("mock_palette".into(), "purple".into());
        overrides.insert("missing".into(), "value".into());

        let report = apply_core_options(&core, "mock-core", overrides);

        assert!(report.applied.is_empty());
        assert_eq!(
            report.unknown.get("missing").map(String::as_str),
            Some("value")
        );
        assert_eq!(report.invalid.len(), 1);
        assert_eq!(report.invalid[0].key, "mock_palette");
    }
}
