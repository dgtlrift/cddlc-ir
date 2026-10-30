//! # cddlc-ir
//!
//! Semantic analysis and IR lowering for `cddlc`.
//!
//! Takes a parsed [`CddlModule`] from `cddlc-parser` and produces a fully
//! resolved [`IrModule`] containing topologically sorted, backend-agnostic
//! type definitions ready for code generation.
//!
//! ## Usage
//!
//! ```rust,no_run
//! use cddlc_parser::parse_cddl;
//! use cddlc_ir::lower;
//! use std::path::PathBuf;
//!
//! let src = r#"
//!     ; @capacity 8
//!     readings = [* float32]
//! "#;
//! let parsed  = parse_cddl(src, PathBuf::from("example.cddl")).unwrap();
//! let lowered = lower(&parsed.value, 16, 64).unwrap();
//!
//! for (name, def) in &lowered.module.types {
//!     println!("{name}: {:?}", std::mem::discriminant(def));
//! }
//! ```

pub mod error;
pub mod lower;
pub mod lower_constraints;
pub mod prelude;
pub mod types;

pub use error::{IrError, IrWarning, IrWarningKind, LowerResult};
pub use lower::lower;
pub use types::{
    AliasDef, ArrayDef, Capacity, CborTag, Constraint, EnumDef, EnumVariant,
    FieldDef, FieldKey, IrModule, LiteralValue, MapDef, Occurrence, Primitive,
    SourceRef, Sources, StructDef, TypeDef, TypeRef,
};
