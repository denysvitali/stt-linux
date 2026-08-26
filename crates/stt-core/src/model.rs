//! Model definitions and downloading.
//!
//! Models are never bundled — they are NVIDIA's, under their own licence — so
//! the first run fetches them from Hugging Face into
//! `~/.local/share/stt-linux/models/`.

use anyhow::{Context, Result, bail};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// One file to fetch.
#[derive(Debug, Clone, Copy)]
pub struct ModelFile {
    pub name: &'static str,
    /// Approximate size, for the progress display before headers arrive.
    pub approx_bytes: u64,
}

/// A downloadable model variant.
#[derive(Debug, Clone, Copy)]
pub struct ModelSpec {
    /// Directory name under the models dir.
    pub dir_name: &'static str,
    /// Hugging Face repo, e.g. `istupakov/parakeet-tdt-0.6b-v3-onnx`.
    pub repo: &'static str,
    pub revision: &'static str,
    pub files: &'static [ModelFile],
    pub description: &'static str,
}

impl ModelSpec {
    pub fn url_for(&self, file: &str) -> String {
        format!(
            "https://huggingface.co/{}/resolve/{}/{file}",
            self.repo, self.revision
        )
    }

    pub fn total_bytes(&self) -> u64 {
        self.files.iter().map(|f| f.approx_bytes).sum()
    }

    /// Whether every file is already present and non-empty.
    pub fn is_complete(&self, dir: &Path) -> bool {
        self.files.iter().all(|f| {
            dir.join(f.name)
                .metadata()
                .is_ok_and(|m| m.is_file() && m.len() > 0)
        })
    }

    pub fn missing_files(&self, dir: &Path) -> Vec<&'static str> {
        self.files
            .iter()
            .filter(|f| {
                !dir.join(f.name)
                    .metadata()
                    .is_ok_and(|m| m.is_file() && m.len() > 0)
            })
            .map(|f| f.name)
            .collect()
    }
}

/// Parakeet TDT v3, int8 weights — the default.
///
/// Only the int8 files are fetched. `parakeet-rs` probes for
/// `encoder-model.onnx` before `encoder-model.int8.onnx`, so downloading the
/// fp32 pair alongside would silently shadow these and quadruple both memory
/// use and latency. Fetching int8 alone is what actually selects int8.
pub const PARAKEET_TDT_V3_INT8: ModelSpec = ModelSpec {
    dir_name: "parakeet-tdt-0.6b-v3",
    repo: "istupakov/parakeet-tdt-0.6b-v3-onnx",
    revision: "main",
    description: "Parakeet TDT 0.6B v3 (int8) — 25 European languages",
    files: &[
        ModelFile {
            name: "encoder-model.int8.onnx",
            approx_bytes: 620_000_000,
        },
        ModelFile {
            name: "decoder_joint-model.int8.onnx",
            approx_bytes: 20_000_000,
        },
        ModelFile {
            name: "vocab.txt",
            approx_bytes: 100_000,
        },
    ],
};

/// Parakeet TDT v3, fp32 weights — the fallback if int8 misbehaves.
pub const PARAKEET_TDT_V3_FP32: ModelSpec = ModelSpec {
    dir_name: "parakeet-tdt-0.6b-v3-fp32",
    repo: "istupakov/parakeet-tdt-0.6b-v3-onnx",
    revision: "main",
    description: "Parakeet TDT 0.6B v3 (fp32) — larger, slower on CPU",
    files: &[
        ModelFile {
            name: "encoder-model.onnx",
            approx_bytes: 2_000_000,
        },
        ModelFile {
            name: "encoder-model.onnx.data",
            approx_bytes: 2_400_000_000,
        },
        ModelFile {
            name: "decoder_joint-model.onnx",
            approx_bytes: 70_000_000,
        },
        ModelFile {
            name: "vocab.txt",
            approx_bytes: 100_000,
        },
    ],
};

pub const ALL_MODELS: &[ModelSpec] = &[PARAKEET_TDT_V3_INT8, PARAKEET_TDT_V3_FP32];

pub fn spec_by_name(name: &str) -> Option<&'static ModelSpec> {
    ALL_MODELS.iter().find(|m| m.dir_name == name)
}

/// Progress callback: `(file_name, bytes_done, total_bytes_or_none)`.
pub type ProgressFn<'a> = &'a mut dyn FnMut(&str, u64, Option<u64>);

/// Download every missing file for `spec` into the models directory.
///
/// Already-present files are skipped, so an interrupted download resumes at
/// file granularity. Each file is written to a `.part` sibling and renamed
/// only on success, so a truncated transfer can never masquerade as a
/// complete model.
pub fn download(spec: &ModelSpec, progress: ProgressFn<'_>) -> Result<PathBuf> {
    let dir = crate::paths::models_dir()?.join(spec.dir_name);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

    for file in spec.files {
        let dest = dir.join(file.name);
        if dest.metadata().is_ok_and(|m| m.is_file() && m.len() > 0) {
            tracing::debug!(file = file.name, "already downloaded");
            continue;
        }
        download_one(&spec.url_for(file.name), &dest, file.name, progress)
            .with_context(|| format!("downloading {}", file.name))?;
    }
    Ok(dir)
}

fn download_one(
    url: &str,
    dest: &Path,
    display_name: &str,
    progress: ProgressFn<'_>,
) -> Result<()> {
    let mut response = ureq::get(url)
        .call()
        .with_context(|| format!("requesting {url}"))?;

    let status = response.status();
    if !status.is_success() {
        bail!("{url} returned HTTP {status}");
    }

    let total = response.body().content_length();
    let part = dest.with_extension("part");
    let mut out =
        std::fs::File::create(&part).with_context(|| format!("creating {}", part.display()))?;

    // No overall size limit: these files are hundreds of megabytes and ureq
    // caps reads at 10 MB by default.
    let mut reader = response.body_mut().with_config().limit(u64::MAX).reader();

    let mut buf = vec![0u8; 256 * 1024];
    let mut done: u64 = 0;
    loop {
        let n = reader.read(&mut buf).context("reading response body")?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n]).context("writing to disk")?;
        done += n as u64;
        progress(display_name, done, total);
    }
    out.flush()?;
    drop(out);

    if let Some(expected) = total
        && done != expected
    {
        let _ = std::fs::remove_file(&part);
        bail!("truncated download: got {done} of {expected} bytes");
    }

    std::fs::rename(&part, dest)
        .with_context(|| format!("renaming {} into place", part.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_point_at_the_right_repo() {
        let url = PARAKEET_TDT_V3_INT8.url_for("vocab.txt");
        assert_eq!(
            url,
            "https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx/resolve/main/vocab.txt"
        );
    }

    #[test]
    fn int8_spec_excludes_fp32_weights() {
        // parakeet-rs prefers `encoder-model.onnx` over the int8 file, so the
        // int8 spec must not ship it or int8 would never be selected.
        let names: Vec<_> = PARAKEET_TDT_V3_INT8.files.iter().map(|f| f.name).collect();
        assert!(names.contains(&"encoder-model.int8.onnx"));
        assert!(!names.contains(&"encoder-model.onnx"));
        assert!(!names.contains(&"encoder-model.onnx.data"));
    }

    #[test]
    fn every_spec_ships_a_vocab() {
        // `ParakeetTDT::from_pretrained` hard-fails without vocab.txt.
        for spec in ALL_MODELS {
            assert!(
                spec.files.iter().any(|f| f.name == "vocab.txt"),
                "{} is missing vocab.txt",
                spec.dir_name
            );
        }
    }

    #[test]
    fn completeness_tracks_files_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let spec = PARAKEET_TDT_V3_INT8;
        assert!(!spec.is_complete(dir.path()));
        assert_eq!(spec.missing_files(dir.path()).len(), spec.files.len());

        for f in spec.files {
            std::fs::write(dir.path().join(f.name), b"x").unwrap();
        }
        assert!(spec.is_complete(dir.path()));
        assert!(spec.missing_files(dir.path()).is_empty());
    }

    #[test]
    fn empty_files_do_not_count_as_present() {
        // A zero-byte file is what a killed download leaves behind.
        let dir = tempfile::tempdir().unwrap();
        for f in PARAKEET_TDT_V3_INT8.files {
            std::fs::write(dir.path().join(f.name), b"").unwrap();
        }
        assert!(!PARAKEET_TDT_V3_INT8.is_complete(dir.path()));
    }

    #[test]
    fn specs_are_addressable_by_name() {
        assert!(spec_by_name("parakeet-tdt-0.6b-v3").is_some());
        assert!(spec_by_name("nonexistent").is_none());
    }
}
