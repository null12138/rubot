use anyhow::Result;
use std::path::{Path, PathBuf};
use std::process::Command;
use tracing::info;

pub struct WorkspaceGit {
    workspace_path: PathBuf,
}

impl WorkspaceGit {
    pub fn new(workspace: &Path) -> Self {
        Self {
            workspace_path: workspace.to_path_buf(),
        }
    }

    /// Initialize git in the workspace if not already present.
    pub fn init(&self) -> Result<()> {
        if !self.workspace_path.join(".git").exists() {
            info!("Initializing git repository in workspace");
            self.run_git(&["init"])?;
            
            // Initial .gitignore for workspace
            let gitignore_path = self.workspace_path.join(".gitignore");
            if !gitignore_path.exists() {
                std::fs::write(gitignore_path, ".DS_Store\n")?;
            }
            
            self.commit("Initial workspace setup")?;
        }
        Ok(())
    }

    /// Add all changes and commit with a message.
    pub fn commit(&self, message: &str) -> Result<()> {
        self.run_git(&["add", "."])?;
        
        // Check if there are changes to commit
        let status = Command::new("git")
            .arg("-C")
            .arg(&self.workspace_path)
            .arg("status")
            .arg("--porcelain")
            .output()?;
            
        if status.stdout.is_empty() {
            return Ok(()); // Nothing to commit
        }

        self.run_git(&["commit", "-m", message])?;
        info!("Workspace commit: {}", message);
        Ok(())
    }

    fn run_git(&self, args: &[&str]) -> Result<()> {
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.workspace_path)
            .args(args)
            .output()?;

        if !output.status.success() {
            let err = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Git error: {}", err);
        }
        Ok(())
    }
}
