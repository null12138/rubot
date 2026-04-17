use anyhow::Result;
use std::path::Path;

use super::user_tool_types::{UserToolManifest, UserToolManifestFile};

const MANIFEST_FILE: &str = "tools/manifest.json";

fn manifest_path(workspace: &Path) -> std::path::PathBuf {
    workspace.join(MANIFEST_FILE)
}

pub fn load(workspace: &Path) -> UserToolManifestFile {
    let path = manifest_path(workspace);
    match std::fs::read_to_string(&path) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_else(|e| {
            tracing::warn!("Manifest corrupted, starting fresh: {}", e);
            UserToolManifestFile::default()
        }),
        Err(_) => UserToolManifestFile::default(),
    }
}

pub fn save(workspace: &Path, manifest: &UserToolManifestFile) -> Result<()> {
    let path = manifest_path(workspace);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(manifest)?;
    std::fs::write(&path, json)?;
    Ok(())
}

pub fn add_tool(workspace: &Path, tool: UserToolManifest) -> Result<()> {
    let mut manifest = load(workspace);
    manifest.tools.push(tool);
    save(workspace, &manifest)
}

pub fn remove_tool(workspace: &Path, name: &str) -> Result<()> {
    let mut manifest = load(workspace);
    manifest.tools.retain(|t| t.name != name);
    save(workspace, &manifest)
}

pub fn find_tool(workspace: &Path, name: &str) -> Option<UserToolManifest> {
    let manifest = load(workspace);
    manifest.tools.into_iter().find(|t| t.name == name)
}

pub fn list_tools(workspace: &Path) -> Vec<UserToolManifest> {
    load(workspace).tools
}
