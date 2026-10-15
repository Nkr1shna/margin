//! Which runtime runs EmbeddingGemma 2 (`docs/plan-linux-service.md` §2.1, §2.11): ONNX
//! Runtime on the CPU (`onnx-community/embeddinggemma-2-ONNX`), in one of the export's
//! weight formats (8-bit by default). The formats give close but not equal vectors, so
//! each has its own `embedder_version`: rows stamped with another version (including the
//! Core ML vectors a Mac service wrote before the service moved to Linux,
//! `LEGACY_COREML_VERSION`) are embedded again, and the dense leg only compares vectors of
//! one version.

use std::fmt;
use std::str::FromStr;

/// The ONNX export's weight formats (its `onnx/model{suffix}.onnx` files).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OnnxDtype {
    /// 8-bit weights (`*_quantized.onnx`, ~0.85 GB for all three graphs): the default;
    /// cosine 0.9999 to fp32 on text in the spike.
    Q8,
    /// Full precision (`*.onnx`, ~2.9 GB): the reference for the eval A/B.
    Fp32,
    /// Half precision (`*_fp16.onnx`). The model card warns that float16 can give NaNs;
    /// kept selectable so the eval can measure it, never the default.
    Fp16,
}

impl OnnxDtype {
    pub const ALL: [OnnxDtype; 3] = [OnnxDtype::Q8, OnnxDtype::Fp32, OnnxDtype::Fp16];

    /// What the export appends to a graph's name for this format.
    pub fn file_suffix(self) -> &'static str {
        match self {
            OnnxDtype::Q8 => "_quantized",
            OnnxDtype::Fp32 => "",
            OnnxDtype::Fp16 => "_fp16",
        }
    }

    /// "q8", "fp32" or "fp16".
    pub fn as_str(self) -> &'static str {
        match self {
            OnnxDtype::Q8 => "q8",
            OnnxDtype::Fp32 => "fp32",
            OnnxDtype::Fp16 => "fp16",
        }
    }
}

/// The version the Core ML backend stamped on passages when the service ran on a Mac.
/// No build runs it any more: rows carrying it are re-embedded by the one-pass re-embed
/// (`margin_service::ingest::reembed`) like any other version that isn't the active one.
pub const LEGACY_COREML_VERSION: &str = "embeddinggemma-2-coreml/title-prefix-v1";

/// The embedding runtime, as `service.toml`'s `embedder` and the pipeline's `embedder`
/// stage name it: `"onnx-q8"`, `"onnx-fp32"` or `"onnx-fp16"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Backend {
    /// ONNX Runtime on the CPU.
    Onnx(OnnxDtype),
}

impl Default for Backend {
    /// The backend unless configured otherwise: ONNX 8-bit.
    fn default() -> Self {
        Backend::Onnx(OnnxDtype::Q8)
    }
}

impl Backend {
    pub const ALL: [Backend; 3] = [
        Backend::Onnx(OnnxDtype::Q8),
        Backend::Onnx(OnnxDtype::Fp32),
        Backend::Onnx(OnnxDtype::Fp16),
    ];

    /// The name configs use.
    pub fn name(self) -> &'static str {
        match self {
            Backend::Onnx(OnnxDtype::Q8) => "onnx-q8",
            Backend::Onnx(OnnxDtype::Fp32) => "onnx-fp32",
            Backend::Onnx(OnnxDtype::Fp16) => "onnx-fp16",
        }
    }

    /// The backend called `name`, if any.
    pub fn parse(name: &str) -> Option<Backend> {
        Backend::ALL.into_iter().find(|b| b.name() == name.trim())
    }

    /// The model, its format and the way inputs are prefixed: stored on every passage as
    /// `embedder`. Bump the `title-prefix` part when the prompts change.
    pub fn embedder_version(self) -> &'static str {
        match self {
            Backend::Onnx(OnnxDtype::Q8) => "embeddinggemma-2-onnx-q8/title-prefix-v1",
            Backend::Onnx(OnnxDtype::Fp32) => "embeddinggemma-2-onnx-fp32/title-prefix-v1",
            Backend::Onnx(OnnxDtype::Fp16) => "embeddinggemma-2-onnx-fp16/title-prefix-v1",
        }
    }

    /// The backend whose vectors carry `version`, if any.
    pub fn of_version(version: &str) -> Option<Backend> {
        Backend::ALL
            .into_iter()
            .find(|b| b.embedder_version() == version)
    }

    /// The Hugging Face repo its files come from.
    pub fn repo(self) -> &'static str {
        crate::ONNX_REPO
    }

    /// The folder under `data_dir/models` its files are kept in. Each ONNX format has its
    /// own, so switching never mixes files.
    pub fn dir_name(self) -> &'static str {
        match self {
            Backend::Onnx(OnnxDtype::Q8) => "embeddinggemma-2-onnx-q8",
            Backend::Onnx(OnnxDtype::Fp32) => "embeddinggemma-2-onnx-fp32",
            Backend::Onnx(OnnxDtype::Fp16) => "embeddinggemma-2-onnx-fp16",
        }
    }

    /// How Settings names it.
    pub fn label(self) -> &'static str {
        match self {
            Backend::Onnx(OnnxDtype::Q8) => "EmbeddingGemma 2 (ONNX, 8-bit)",
            Backend::Onnx(OnnxDtype::Fp32) => "EmbeddingGemma 2 (ONNX, 32-bit)",
            Backend::Onnx(OnnxDtype::Fp16) => "EmbeddingGemma 2 (ONNX, 16-bit)",
        }
    }
}

impl fmt::Display for Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// An `embedder` name that isn't a backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownBackend(pub String);

impl fmt::Display for UnknownBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let known: Vec<&str> = Backend::ALL.iter().map(|b| b.name()).collect();
        write!(
            f,
            "unknown embedder {:?} (one of {})",
            self.0,
            known.join(", ")
        )
    }
}

impl std::error::Error for UnknownBackend {}

impl FromStr for Backend {
    type Err = UnknownBackend;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Backend::parse(s).ok_or_else(|| UnknownBackend(s.to_string()))
    }
}

/// `backend`'s version (`Backend::embedder_version`), for the service and the eval runner.
pub fn embedder_version(backend: Backend) -> &'static str {
    backend.embedder_version()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip_and_versions_differ() {
        for b in Backend::ALL {
            assert_eq!(Backend::parse(b.name()), Some(b));
            assert_eq!(b.name().parse::<Backend>().unwrap(), b);
            assert_eq!(b.to_string(), b.name());
            assert_eq!(Backend::of_version(b.embedder_version()), Some(b));
        }
        let mut versions: Vec<_> = Backend::ALL.iter().map(|b| b.embedder_version()).collect();
        versions.push(LEGACY_COREML_VERSION);
        versions.sort();
        versions.dedup();
        assert_eq!(versions.len(), Backend::ALL.len() + 1);
        let mut dirs: Vec<_> = Backend::ALL.iter().map(|b| b.dir_name()).collect();
        dirs.sort();
        dirs.dedup();
        assert_eq!(dirs.len(), Backend::ALL.len());
        let err = "onnx".parse::<Backend>().unwrap_err();
        assert!(err.to_string().contains("onnx-q8"), "{err}");
        assert_eq!(
            Backend::parse(" onnx-fp32 "),
            Some(Backend::Onnx(OnnxDtype::Fp32))
        );
    }

    #[test]
    fn core_ml_is_gone_but_its_version_is_unchanged() {
        // Stored on every passage the Mac service wrote: re-embedded, never read as current.
        assert_eq!(
            LEGACY_COREML_VERSION,
            "embeddinggemma-2-coreml/title-prefix-v1"
        );
        assert_eq!(Backend::of_version(LEGACY_COREML_VERSION), None);
        assert!("coreml".parse::<Backend>().is_err());
    }

    #[test]
    fn the_default_is_onnx_8_bit() {
        assert_eq!(Backend::default(), Backend::Onnx(OnnxDtype::Q8));
        assert_eq!(
            Backend::default().embedder_version(),
            crate::EMBEDDER_VERSION
        );
    }
}
