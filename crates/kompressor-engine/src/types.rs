//! Vocabulary shared by detection, routing, compression and analytics.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Compression mode.
///
/// `Recoverable` is the default, and deliberately so: a code default of
/// `aggressive` that every real deployment overrides by environment is a
/// standing trap, because a probe run against the bare default "proves" that
/// handles are never minted. The shipped default is the recommended one.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Lossy, no fragment retained: elisions are announced but not fetchable.
    Aggressive,
    /// Reversible-by-construction transforms only.
    Lossless,
    /// The aggressive engine, with every elision stashed behind a `ref:k_…`.
    #[default]
    Recoverable,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Aggressive => "aggressive",
            Mode::Lossless => "lossless",
            Mode::Recoverable => "recoverable",
        }
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Mode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "aggressive" => Ok(Mode::Aggressive),
            "lossless" => Ok(Mode::Lossless),
            "recoverable" => Ok(Mode::Recoverable),
            other => Err(format!("unknown compression mode: {other}")),
        }
    }
}

/// What the detector decided a payload is.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentType {
    JsonArray,
    SourceCode,
    SearchResults,
    BuildOutput,
    GitDiff,
    Html,
    Tabular,
    Listing,
    Config,
    PlainText,
}

impl ContentType {
    pub fn as_str(self) -> &'static str {
        match self {
            ContentType::JsonArray => "json_array",
            ContentType::SourceCode => "source_code",
            ContentType::SearchResults => "search",
            ContentType::BuildOutput => "build",
            ContentType::GitDiff => "diff",
            ContentType::Html => "html",
            ContentType::Tabular => "tabular",
            ContentType::Listing => "listing",
            ContentType::Config => "config",
            ContentType::PlainText => "text",
        }
    }

    /// The analytics vocabulary: a type reads the same whether it arrived as a
    /// whole message or as one span inside a mixed body.
    pub fn kind(self) -> Kind {
        match self {
            ContentType::JsonArray => Kind::Json,
            ContentType::BuildOutput => Kind::Log,
            ContentType::PlainText => Kind::Text,
            ContentType::SourceCode => Kind::SourceCode,
            ContentType::SearchResults => Kind::Search,
            ContentType::GitDiff => Kind::Diff,
            ContentType::Html => Kind::Html,
            ContentType::Tabular => Kind::Tabular,
            ContentType::Listing => Kind::Listing,
            ContentType::Config => Kind::Config,
        }
    }
}

/// What gets reported in `by_type` / `content_types`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Json,
    Log,
    Text,
    Search,
    Diff,
    Tabular,
    Listing,
    SourceCode,
    Config,
    Html,
    /// A heterogeneous body whose structured spans were compressed in place.
    /// Never reported on its own: the route unfolds it into the real types.
    Mixed,
    /// A shrink that changed layout only, declared rather than passed off as
    /// an elision.
    Reformat,
    /// A reversible columnar envelope (lossless factoring, and the same
    /// transform when it runs in a lossy mode).
    Factored,
    /// A file Read whose content a later edit invalidated.
    StaleRead,
    /// A span an earlier message in the same conversation already showed.
    CrossTurn,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Json => "json",
            Kind::Log => "log",
            Kind::Text => "text",
            Kind::Search => "search",
            Kind::Diff => "diff",
            Kind::Tabular => "tabular",
            Kind::Listing => "listing",
            Kind::SourceCode => "source_code",
            Kind::Config => "config",
            Kind::Html => "html",
            Kind::Mixed => "mixed",
            Kind::Reformat => "reformat",
            Kind::Factored => "factored",
            Kind::StaleRead => "stale_read",
            Kind::CrossTurn => "cross_turn",
        }
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which compressor a detected type routes to.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Strategy {
    SmartCrusher,
    Search,
    Log,
    Diff,
    CodeAware,
    Tabular,
    Listing,
    Config,
    Passthrough,
}

/// Languages the code compressor has a grammar for.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Language {
    Python,
    JavaScript,
    TypeScript,
    Go,
    Rust,
    Java,
    CSharp,
    C,
    Cpp,
}

impl Language {
    pub fn as_str(self) -> &'static str {
        match self {
            Language::Python => "python",
            Language::JavaScript => "javascript",
            Language::TypeScript => "typescript",
            Language::Go => "go",
            Language::Rust => "rust",
            Language::Java => "java",
            Language::CSharp => "csharp",
            Language::C => "c",
            Language::Cpp => "cpp",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigFlavor {
    Yaml,
    Toml,
    Ini,
}

impl ConfigFlavor {
    pub fn as_str(self) -> &'static str {
        match self {
            ConfigFlavor::Yaml => "yaml",
            ConfigFlavor::Toml => "toml",
            ConfigFlavor::Ini => "ini",
        }
    }
}

/// Everything the detector learned, including what it learned on the way.
#[derive(Clone, Debug, Default, Serialize)]
pub struct DetectionMeta {
    pub item_count: Option<usize>,
    pub concatenated_json: bool,
    pub sample_truncated: bool,
    pub diagnostics: bool,
    pub untimestamped: bool,
    pub uniform_stream: bool,
    /// A uniform stream that nevertheless carries error signal, and so must be
    /// routed to the compressor that scores errors rather than the one that
    /// samples head and tail. Set only on the uniform-stream path, which is
    /// where a crash in the middle of a progress log was being elided with no
    /// hint it had happened.
    pub error_signal: bool,
    pub language: Option<Language>,
    /// Disassembly or assembler source: code with no grammar, which nothing
    /// may fold or sample, the fold fallback included.
    pub assembly: bool,
    pub flavor: Option<ConfigFlavor>,
    pub skipped_size: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct Detection {
    pub content_type: ContentType,
    pub confidence: f32,
    pub meta: DetectionMeta,
}

impl Detection {
    pub fn new(content_type: ContentType, confidence: f32) -> Self {
        Detection {
            content_type,
            confidence,
            meta: DetectionMeta::default(),
        }
    }

    pub fn with_meta(content_type: ContentType, confidence: f32, meta: DetectionMeta) -> Self {
        Detection {
            content_type,
            confidence,
            meta,
        }
    }
}

/// Conversation context a compressor may consult.
///
/// `preceding_user_text` is present but deliberately **not** read by any
/// compressor. Ranking a payload against the question that prompted it is the
/// obvious improvement and it is a cache-buster: an agent re-sends its history
/// every turn under a different latest question, so query-dependent bytes
/// change a prefix the provider is caching. The field stays so the decision is
/// visible rather than merely absent, and the corpus harness asserts that no
/// payload becomes query-dependent.
#[derive(Clone, Debug, Default)]
pub struct Context<'a> {
    pub role: &'a str,
    pub message_index: Option<usize>,
    pub preceding_user_text: &'a str,
    pub source_tool: Option<&'a str>,
    pub file_path: Option<&'a str>,
}

impl<'a> Context<'a> {
    pub fn for_role(role: &'a str) -> Self {
        Context {
            role,
            ..Default::default()
        }
    }
}

/// A content type forced by the caller, bypassing detection.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForcedType {
    Json,
    Log,
    Text,
    Config,
}

impl std::str::FromStr for ForcedType {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "json" | "json_array" => Ok(ForcedType::Json),
            "log" | "build" => Ok(ForcedType::Log),
            "text" => Ok(ForcedType::Text),
            "config" => Ok(ForcedType::Config),
            other => Err(format!("unknown content type: {other}")),
        }
    }
}
