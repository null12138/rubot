use anyhow::Result;
use async_trait::async_trait;
use flate2::write::GzEncoder;
use flate2::Compression;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::registry::{Tool, ToolResult};

/// Online LaTeX → PDF compiler. Builds an in-memory `tar.gz` containing
/// `main.tex` and POSTs it to latexonline.cc's `/data` endpoint, which
/// wraps TeX Live and returns the compiled PDF. Used instead of local
/// pdflatex/xelatex (not installed in the sandbox).
pub struct LatexPdf {
    workspace: PathBuf,
}

impl LatexPdf {
    pub fn new(ws: &Path) -> Self {
        Self {
            workspace: ws.to_path_buf(),
        }
    }
}

/// Wrap tex source in a `main.tex`-only tar.gz suitable for the /data endpoint.
fn build_tarball(tex: &str) -> Result<Vec<u8>> {
    let mut gz = GzEncoder::new(Vec::new(), Compression::default());
    {
        let mut tar_writer = tar::Builder::new(&mut gz);
        let bytes = tex.as_bytes();
        let mut header = tar::Header::new_gnu();
        header.set_path("main.tex")?;
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar_writer.append(&header, bytes)?;
        tar_writer.finish()?;
    }
    Ok(gz.finish()?)
}

#[async_trait]
impl Tool for LatexPdf {
    fn name(&self) -> &str {
        "latex_pdf"
    }

    fn description(&self) -> &str {
        "Compile LaTeX source to PDF via an online service (no local LaTeX install needed). \
         Saves the PDF to the configured workspace `files/` directory as <name>.pdf and returns its absolute path. \
         Use compiler=xelatex for Chinese/CJK, pdflatex for pure English/math."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "tex":      { "type": "string", "description": "Full LaTeX source code" },
                "name":     { "type": "string", "description": "Output filename without extension" },
                "compiler": {
                    "type": "string",
                    "enum": ["pdflatex", "xelatex", "lualatex"],
                    "description": "LaTeX engine (default xelatex for CJK-safe output)"
                }
            },
            "required": ["tex", "name"]
        })
    }

    async fn execute(&self, params: serde_json::Value) -> Result<ToolResult> {
        let tex = params["tex"].as_str().unwrap_or("").to_string();
        let name = params["name"].as_str().unwrap_or("output");
        let compiler = params["compiler"].as_str().unwrap_or("xelatex");

        if tex.trim().is_empty() {
            return Ok(ToolResult::err("tex is empty".into()));
        }
        if !matches!(compiler, "pdflatex" | "xelatex" | "lualatex") {
            return Ok(ToolResult::err(format!("invalid compiler '{}'", compiler)));
        }

        let tarball = match build_tarball(&tex) {
            Ok(t) => t,
            Err(e) => return Ok(ToolResult::err(format!("tarball build failed: {}", e))),
        };

        let url = format!(
            "https://latexonline.cc/data?target=main.tex&command={}&force=true",
            compiler
        );
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()?;

        // latexonline.cc /data expects multipart form upload with a single
        // "file" part containing a tar.gz of the project (must include main.tex).
        let part = reqwest::multipart::Part::bytes(tarball)
            .file_name("project.tar.gz")
            .mime_str("application/gzip")?;
        let form = reqwest::multipart::Form::new().part("file", part);

        let res = client.post(&url).multipart(form).send().await?;
        let status = res.status();
        let bytes = res.bytes().await?;

        if !status.is_success() {
            let msg: String = String::from_utf8_lossy(&bytes).chars().take(500).collect();
            return Ok(ToolResult::err(format!(
                "HTTP {}: {}",
                status.as_u16(),
                msg
            )));
        }
        if bytes.len() < 4 || &bytes[..4] != b"%PDF" {
            let msg: String = String::from_utf8_lossy(&bytes).chars().take(500).collect();
            return Ok(ToolResult::err(format!(
                "compilation failed (no PDF returned): {}",
                msg
            )));
        }

        let out_dir = self.workspace.join("files");
        let _ = tokio::fs::create_dir_all(&out_dir).await;
        let safe: String = name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let out_path = out_dir.join(format!("{}.pdf", safe));
        tokio::fs::write(&out_path, &bytes).await?;
        let abs = out_path.canonicalize().unwrap_or(out_path);

        Ok(ToolResult::ok(format!(
            "PDF compiled successfully ({} bytes).\n[Generated files — absolute paths are directly accessible to the user. Reference these paths in your reply; use `[FILE: /abs/path]` markers for auto-attachment on Telegram.]\n- {} ({} bytes)",
            bytes.len(),
            abs.display(),
            bytes.len()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::build_tarball;

    #[test]
    fn tarball_has_gzip_magic() {
        let bytes =
            build_tarball(r"\documentclass{article}\begin{document}Hi\end{document}").unwrap();
        assert!(bytes.len() > 20);
        assert_eq!(&bytes[..2], &[0x1f, 0x8b], "not gzip");
    }

    #[test]
    fn tarball_embeds_main_tex() {
        use flate2::read::GzDecoder;
        use std::io::Read;
        let src = r"\documentclass{article}\begin{document}Round-trip\end{document}";
        let bytes = build_tarball(src).unwrap();
        let gz = GzDecoder::new(&bytes[..]);
        let mut ar = tar::Archive::new(gz);
        let mut found = false;
        for entry in ar.entries().unwrap() {
            let mut e = entry.unwrap();
            assert_eq!(e.path().unwrap().to_str().unwrap(), "main.tex");
            let mut contents = String::new();
            e.read_to_string(&mut contents).unwrap();
            assert_eq!(contents, src);
            found = true;
        }
        assert!(found);
    }
}
