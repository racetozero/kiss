use anyhow::{Context as _, Result};
use kiss_ai::Model;
use kiss_coding::{SessionManager, Settings};
use serde_json::{Value, json};
use std::io::Write as _;
use std::path::PathBuf;

pub fn export(
    description: &str,
    settings: &Settings,
    model: &Model,
    manager: &SessionManager,
) -> Result<PathBuf> {
    let home = dirs::home_dir().context(
        "could not create a bug report because the home directory is unavailable; set HOME and run /bug again",
    )?;
    let directory = home.join(".kiss/agent/bug-reports");
    std::fs::create_dir_all(&directory).with_context(|| {
        format!(
            "could not create bug-report directory {}; check its permissions and run /bug again",
            directory.display()
        )
    })?;
    let mut safe_settings = serde_json::to_value(settings)?;
    redact(&mut safe_settings);
    let path = directory.join(format!(
        "{}-{}.json",
        chrono::Utc::now().format("%Y%m%d-%H%M%S-%6f"),
        std::process::id()
    ));
    let report = json!({
        "description": description,
        "kissVersion": env!("CARGO_PKG_VERSION"),
        "platform": {"os": std::env::consts::OS, "arch": std::env::consts::ARCH},
        "model": {"provider": model.provider, "id": model.id, "api": model.api},
        "session": {
            "id": manager.session_id(),
            "cwd": manager.cwd(),
            "file": manager.session_file(),
        },
        "settings": safe_settings,
        "transcriptIncluded": false,
    });
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .with_context(|| {
            format!(
                "could not create bug report {}; run /bug again to use a new report name",
                path.display()
            )
        })?;
    file.write_all(&serde_json::to_vec_pretty(&report)?)
        .with_context(|| {
        format!(
            "could not write bug report {}; check available space and permissions, then run /bug again",
            path.display()
        )
    })?;
    Ok(path)
}

fn redact(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                let key = key.to_ascii_lowercase();
                if ["key", "token", "secret", "password", "authorization"]
                    .iter()
                    .any(|needle| key.contains(needle))
                {
                    *value = Value::String("[redacted]".into());
                } else {
                    redact(value);
                }
            }
        }
        Value::Array(values) => values.iter_mut().for_each(redact),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_are_redacted_recursively() {
        let mut value = json!({"apiKey": "one", "nested": {"authToken": "two", "safe": 3}});
        redact(&mut value);
        assert_eq!(value["apiKey"], "[redacted]");
        assert_eq!(value["nested"]["authToken"], "[redacted]");
        assert_eq!(value["nested"]["safe"], 3);
    }
}
