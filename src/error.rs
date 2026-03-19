use std::path::PathBuf;
use thiserror::Error;

/// All errors that can be produced during IR lowering.
#[derive(Debug, Error)]
pub enum IrError {
    // ── Name resolution ───────────────────────────────────────────────────────

    #[error("undefined type '{name}' referenced in rule '{in_rule}'")]
    UndefinedType { name: String, in_rule: String },

    #[error("duplicate rule name '{name}': first defined at {first}, redefined at {second}")]
    DuplicateRule {
        name:   String,
        first:  String,
        second: String,
    },

    // ── Generics ──────────────────────────────────────────────────────────────

    #[error("generic rule '{name}' requires {expected} type argument(s), got {got}")]
    GenericArgCount { name: String, expected: usize, got: usize },

    #[error("type '{name}' is not generic but was given type arguments")]
    NotGeneric { name: String },

    // ── Import / file loading ─────────────────────────────────────────────────

    #[error("import cycle detected: {}", .cycle.join(" → "))]
    ImportCycle { cycle: Vec<String> },

    #[error("cannot resolve import '{path}': {reason}")]
    ImportNotFound { path: String, reason: String },

    #[error("I/O error reading '{path}': {source}")]
    Io {
        path:   PathBuf,
        #[source]
        source: std::io::Error,
    },

    // ── Constraint lowering ───────────────────────────────────────────────────

    #[error("invalid .size constraint on '{rule}': {reason}")]
    BadSizeConstraint { rule: String, reason: String },

    #[error("invalid range constraint on '{rule}': {reason}")]
    BadRangeConstraint { rule: String, reason: String },

    #[error("control operator '{op}' is not valid on type '{ty}' in rule '{rule}'")]
    InvalidControlOp { op: String, ty: String, rule: String },

    // ── Structural ────────────────────────────────────────────────────────────

    #[error("rule '{name}' cannot be both a type rule and a group rule")]
    AmbiguousRule { name: String },

    #[error("rule '{name}' has incompatible extension: {reason}")]
    IncompatibleExtension { name: String, reason: String },

    // ── Parse forwarding ─────────────────────────────────────────────────────

    #[error("parse error: {0}")]
    Parse(#[from] cddlc_parser::ParseError),
}

/// Non-fatal diagnostic emitted during lowering.
#[derive(Debug, Clone)]
pub struct IrWarning {
    pub rule:    Option<String>,
    pub message: String,
    pub kind:    IrWarningKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IrWarningKind {
    /// Unknown pragma encountered on a rule.
    UnknownPragma,
    /// A rule is marked `@deprecated`.
    DeprecatedRule,
    /// An unbounded occurrence with no `@capacity` pragma — using default.
    DefaultCapacityUsed,
    /// A `.regexp` control op has no `@regex-hook` — validation will be skipped.
    NoRegexHook,
}

impl std::fmt::Display for IrWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.rule {
            Some(r) => write!(f, "warning[{:?}] in '{}': {}", self.kind, r, self.message),
            None    => write!(f, "warning[{:?}]: {}", self.kind, self.message),
        }
    }
}

/// Accumulated result of lowering.
pub struct LowerResult {
    pub module:   crate::types::IrModule,
    pub warnings: Vec<IrWarning>,
}
