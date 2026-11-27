use super::Entry;
use anyhow::{Context, Result, bail};
use std::path::Path;
use toml_edit::DocumentMut;

const ORIGINAL_PROVIDER_MARKER: &str = "# >>> Noria original model provider (managed)\n";

pub(super) fn activate(
    home: &Path,
    exe: &Path,
    address: &str,
    bytes: &[u8],
    entry: &mut Entry,
) -> Result<Vec<u8>> {
    let original = std::str::from_utf8(bytes).context("invalid TOML UTF-8")?;
    let root = original.parse::<DocumentMut>().context("invalid TOML")?;
    if original.contains(ORIGINAL_PROVIDER_MARKER) {
        bail!("the Codex config already contains a Noria routing marker");
    }
    if root
        .get("model_providers")
        .and_then(|v| v.get("noria"))
        .is_some()
    {
        bail!(
            "a noria provider already exists; use integration status or remove the previous manual definition first"
        );
    }
    let mut lines: Vec<&str> = original.split_inclusive('\n').collect();
    entry.root_line.clear();
    let mut section = false;
    let mut found = false;
    for line in &mut lines {
        let trim = line.trim();
        if trim.starts_with('[') {
            section = true;
        }
        if section {
            continue;
        }
        if trim
            .split_once('=')
            .is_some_and(|(key, _)| key.trim() == "model_provider")
        {
            entry.root_line = (*line).into();
            *line = ORIGINAL_PROVIDER_MARKER;
            found = true;
            break;
        }
    }
    if root.contains_key("model_provider") && !found {
        bail!(
            "nonstandard model_provider declaration; edit it to a standalone unquoted key before integration"
        );
    }
    entry.root_block =
        "# >>> Noria routing (managed)\nmodel_provider = \"noria\"\n# <<< Noria routing\n".into();
    let quote = |value: &str| -> String { toml_edit::Value::from(value).to_string() };
    entry.definition = format!(
        "\n# >>> Noria provider (managed)\n[model_providers.noria]\nname = \"Noria\"\nbase_url = {}\nwire_api = \"responses\"\nsupports_websockets = true\n[model_providers.noria.auth]\ncommand = {}\nargs = [\"auth-token\", \"--home\", {}]\n# <<< Noria provider\n",
        quote(&format!("http://{address}/codex/v1")),
        quote(&exe.to_string_lossy()),
        quote(&home.to_string_lossy())
    );
    let source = format!("{}{}{}", entry.root_block, lines.concat(), entry.definition);
    source.parse::<DocumentMut>()?;
    entry.active = true;
    Ok(source.into_bytes())
}

pub(super) fn deactivate(bytes: &[u8], entry: &mut Entry) -> Result<Vec<u8>> {
    let mut source = std::str::from_utf8(bytes)?.to_owned();
    for block in [&entry.root_block, &entry.definition] {
        if block.is_empty() || source.matches(block).count() != 1 {
            bail!(
                "managed routing was edited externally; refusing to overwrite it. Restore the Noria fields or remove integration manually"
            );
        }
        source = source.replacen(block, "", 1);
    }
    if !entry.root_line.is_empty() {
        if source.matches(ORIGINAL_PROVIDER_MARKER).count() != 1 {
            bail!(
                "original Codex provider marker was changed; refusing to overwrite the user's settings"
            );
        }
        source = source.replacen(ORIGINAL_PROVIDER_MARKER, &entry.root_line, 1);
    }
    source.parse::<DocumentMut>()?;
    entry.active = false;
    Ok(source.into_bytes())
}
