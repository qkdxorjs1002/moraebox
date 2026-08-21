use std::{collections::BTreeMap, ffi::OsString};

use moraebox_core::RunSpec;

use crate::{BackendError, EnvironmentComponent};

pub(crate) fn resolve_environment(
    spec: &RunSpec,
) -> Result<BTreeMap<String, String>, BackendError> {
    if !spec.inherit_env {
        return Ok(spec.env.clone());
    }
    resolve_environment_from(std::env::vars_os(), &spec.env)
}

fn resolve_environment_from(
    host: impl IntoIterator<Item = (OsString, OsString)>,
    explicit: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, BackendError> {
    let mut resolved = BTreeMap::new();
    for (name, value) in host {
        let name = name
            .into_string()
            .map_err(|_| BackendError::NonUnicodeEnvironment {
                variable: "<non-Unicode name>".into(),
                component: EnvironmentComponent::Name,
            })?;
        let value = value
            .into_string()
            .map_err(|_| BackendError::NonUnicodeEnvironment {
                variable: name.clone(),
                component: EnvironmentComponent::Value,
            })?;
        resolved.insert(name, value);
    }
    resolved.extend(explicit.clone());
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_values_override_inherited_values() {
        let explicit = BTreeMap::from([
            ("EXPLICIT".into(), "only".into()),
            ("SHARED".into(), "explicit".into()),
        ]);
        let resolved = resolve_environment_from(
            [
                (OsString::from("HOST"), OsString::from("value")),
                (OsString::from("SHARED"), OsString::from("host")),
            ],
            &explicit,
        )
        .unwrap();

        assert_eq!(resolved.get("HOST").map(String::as_str), Some("value"));
        assert_eq!(resolved.get("EXPLICIT").map(String::as_str), Some("only"));
        assert_eq!(resolved.get("SHARED").map(String::as_str), Some("explicit"));
    }

    #[test]
    fn disabled_inheritance_returns_only_explicit_values() {
        let mut spec = RunSpec::command(["true"]);
        spec.env.insert("ONLY".into(), "explicit".into());

        assert_eq!(resolve_environment(&spec).unwrap(), spec.env);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_non_unicode_names_without_exposing_bytes() {
        use std::os::unix::ffi::OsStringExt;

        let error = resolve_environment_from(
            [(OsString::from_vec(vec![0xff]), OsString::from("value"))],
            &BTreeMap::new(),
        )
        .unwrap_err();

        assert!(matches!(
            error,
            BackendError::NonUnicodeEnvironment {
                component: EnvironmentComponent::Name,
                ref variable,
            } if variable == "<non-Unicode name>"
        ));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_non_unicode_values_without_exposing_the_value() {
        use std::os::unix::ffi::OsStringExt;

        let error = resolve_environment_from(
            [(OsString::from("SECRET"), OsString::from_vec(vec![0xff]))],
            &BTreeMap::new(),
        )
        .unwrap_err();

        assert!(matches!(
            error,
            BackendError::NonUnicodeEnvironment {
                component: EnvironmentComponent::Value,
                ref variable,
            } if variable == "SECRET"
        ));
        assert!(!error.to_string().contains('�'));
    }
}
