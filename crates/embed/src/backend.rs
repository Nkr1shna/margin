//! Which runtime runs EmbeddingGemma 2 (`docs/plan-linux-service.md` §2.1, §2.11): Core
//! ML on macOS (`FluidInference/embeddinggemma-2-coreml`, fp16) or ONNX Runtime on the CPU
//! (`onnx-community/embeddinggemma-2-ONNX`, 8-bit by default). The two give close but not
//! equal vectors, so each has its own `embedder_version`: rows stamped with another
//! version are embedded again, and the dense leg only compares vectors of one version.

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

/// The embedding runtime, as `service.toml`'s `embedder` and the pipeline's `embedder`
/// stage name it: `"coreml"`, `"onnx-q8"`, `"onnx-fp32"` or `"onnx-fp16"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Backend {
    /// Core ML (Neural Engine and GPU). macOS only.
    CoreMl,
    /// ONNX Runtime on the CPU. Linux always; macOS with the `onnx` feature.
    Onnx(OnnxDtype),
}

impl Default for Backend {
    /// The backend this target runs unless configured otherwise: Core ML on macOS, ONNX
    /// 8-bit everywhere else.
    fn default() -> Self {
        if cfg!(target_os = "macos") {
            Backend::CoreMl
        } else {
            Backend::Onnx(OnnxDtype::Q8)
        }
    }
}

impl Backend {
    pub const ALL: [Backend; 4] = [
        Backend::CoreMl,
        Backend::Onnx(OnnxDtype::Q8),
        Backend::Onnx(OnnxDtype::Fp32),
        Backend::Onnx(OnnxDtype::Fp16),
    ];

    /// The name configs use.
    pub fn name(self) -> &'static str {
        match self {
            Backend::CoreMl => "coreml",
            Backend::Onnx(OnnxDtype::Q8) => "onnx-q8",
            Backend::Onnx(OnnxDtype::Fp32) => "onnx-fp32",
            Backend::Onnx(OnnxDtype::Fp16) => "onnx-fp16",
        }
    }

    /// The backend called `name`, if any.
    pub fn parse(name: &str) -> Option<Backend> {
        Backend::ALL.into_iter().find(|b| b.name() == name.trim())
    }

    /// Whether this binary can run it: Core ML on macOS; ONNX on Linux, or on macOS
    /// built with the `onnx` feature.
    pub fn is_built(self) -> bool {
        match self {
            Backend::CoreMl => cfg!(target_os = "macos"),
            Backend::Onnx(_) => cfg!(any(target_os = "linux", feature = "onnx")),
        }
    }

    /// The model, its format and the way inputs are prefixed: stored on every passage as
    /// `embedder`. Bump the `title-prefix` part when the prompts change.
    pub fn embedder_version(self) -> &'static str {
        match self {
            Backend::CoreMl => "embeddinggemma-2-coreml/title-prefix-v1",
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
        match self {
            Backend::CoreMl => crate::REPO,
            Backend::Onnx(_) => crate::ONNX_REPO,
        }
    }

    /// The folder under `data_dir/models` its files are kept in. Each backend (and each
    /// ONNX format) has its own, so switching never mixes files.
    pub fn dir_name(self) -> &'static str {
        match self {
            Backend::CoreMl => "embeddinggemma-2",
            Backend::Onnx(OnnxDtype::Q8) => "embeddinggemma-2-onnx-q8",
            Backend::Onnx(OnnxDtype::Fp32) => "embeddinggemma-2-onnx-fp32",
            Backend::Onnx(OnnxDtype::Fp16) => "embeddinggemma-2-onnx-fp16",
        }
    }

    /// How Settings names it.
    pub fn label(self) -> &'static str {
        match self {
            Backend::CoreMl => "EmbeddingGemma 2 (Core ML)",
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
        versions.sort();
        versions.dedup();
        assert_eq!(versions.len(), Backend::ALL.len());
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
    fn the_core_ml_version_is_unchanged() {
        // Stored on every passage the Mac has written: changing it re-embeds everything.
        assert_eq!(
            Backend::CoreMl.embedder_version(),
            "embeddinggemma-2-coreml/title-prefix-v1"
        );
        assert_eq!(Backend::CoreMl.embedder_version(), crate::EMBEDDER_VERSION);
        assert_eq!(Backend::CoreMl.dir_name(), "embeddinggemma-2");
    }

    #[test]
    fn the_default_is_per_target_and_built() {
        let b = Backend::default();
        assert!(b.is_built());
        if cfg!(target_os = "macos") {
            assert_eq!(b, Backend::CoreMl);
        } else {
            assert_eq!(b, Backend::Onnx(OnnxDtype::Q8));
            assert!(!Backend::CoreMl.is_built());
        }
        assert_eq!(
            Backend::Onnx(OnnxDtype::Q8).is_built(),
            cfg!(any(target_os = "linux", feature = "onnx"))
        );
    }
}
