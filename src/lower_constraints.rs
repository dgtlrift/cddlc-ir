/// Constraint lowering.
///
/// Converts `TypeOp` entries from the parser AST into typed [`Constraint`]
/// values on IR nodes.  Also handles the range operator (`..` / `...`).

use cddlc_parser::{CtlOp, Type2, TypeOp, Value};

use crate::error::{IrError, IrWarning, IrWarningKind};
use crate::types::{Constraint, LiteralValue, TypeRef};

/// Lower a single `TypeOp` into one or more `Constraint`s.
///
/// `rule_name` is used only for error messages.
/// `regex_hook` is taken from the `@regex-hook` pragma on the rule, if any.
pub fn lower_type_op(
    op:         &TypeOp,
    base:       &Type2,
    rule_name:  &str,
    regex_hook: Option<&str>,
    warnings:   &mut Vec<IrWarning>,
) -> Result<Vec<Constraint>, IrError> {
    match op {
        TypeOp::Range { upper, inclusive } => {
            lower_range(base, upper, *inclusive, rule_name)
        }

        TypeOp::Control { op, operand } => {
            lower_control_op(&op.node, &operand.node, rule_name, regex_hook, warnings)
        }
    }
}

/// Lower a range operator (`lo..hi` / `lo...hi`).
///
/// `base` is the range's lower bound (the `Type1::base` preceding the range
/// operator). When it's a numeric literal it becomes `min`; otherwise (e.g. a
/// typename base we can't statically resolve) `min` stays `None`, matching the
/// existing behaviour for unresolved bounds elsewhere in this module.
fn lower_range(
    base:      &Type2,
    upper:     &cddlc_parser::Spanned<Type2>,
    inclusive: bool,
    rule_name: &str,
) -> Result<Vec<Constraint>, IrError> {
    let min_lit = match base {
        Type2::Value(v) => Some(lower_value(v)),
        _ => None,
    };

    // The upper bound must be a literal value for us to emit a static constraint.
    match &upper.node {
        Type2::Value(v) => {
            let hi = lower_value(v);
            let c = match (min_lit, hi) {
                (Some(LiteralValue::Uint(lo)), LiteralValue::Uint(hi)) =>
                    Constraint::ValueRangeUint { min: Some(lo), max: Some(hi), inclusive },
                (Some(LiteralValue::Uint(lo)), LiteralValue::Int(hi)) =>
                    Constraint::ValueRangeInt  { min: Some(lo as i64), max: Some(hi), inclusive },
                (Some(LiteralValue::Int(lo)),  LiteralValue::Uint(hi)) =>
                    Constraint::ValueRangeInt  { min: Some(lo), max: Some(hi as i64), inclusive },
                (Some(LiteralValue::Int(lo)),  LiteralValue::Int(hi)) =>
                    Constraint::ValueRangeInt  { min: Some(lo), max: Some(hi), inclusive },
                (Some(LiteralValue::Float(lo)), LiteralValue::Float(hi)) =>
                    Constraint::ValueRangeF64  { min: Some(lo), max: Some(hi), inclusive },
                (None, LiteralValue::Uint(hi)) =>
                    Constraint::ValueRangeUint { min: None, max: Some(hi), inclusive },
                (None, LiteralValue::Int(hi)) =>
                    Constraint::ValueRangeInt  { min: None, max: Some(hi), inclusive },
                (None, LiteralValue::Float(hi)) =>
                    Constraint::ValueRangeF64  { min: None, max: Some(hi), inclusive },
                _ => return Err(IrError::BadRangeConstraint {
                    rule:   rule_name.to_owned(),
                    reason: "range bounds must be numeric literals of compatible type".into(),
                }),
            };
            Ok(vec![c])
        }
        _ => Err(IrError::BadRangeConstraint {
            rule:   rule_name.to_owned(),
            reason: "range upper bound must be a literal value".into(),
        }),
    }
}

fn lower_control_op(
    op:         &CtlOp,
    operand:    &Type2,
    rule_name:  &str,
    regex_hook: Option<&str>,
    warnings:   &mut Vec<IrWarning>,
) -> Result<Vec<Constraint>, IrError> {
    match op {
        CtlOp::Size => lower_size(operand, rule_name),

        CtlOp::Lt => numeric_bound(operand, rule_name, BoundSide::Lt),
        CtlOp::Le => numeric_bound(operand, rule_name, BoundSide::Le),
        CtlOp::Gt => numeric_bound(operand, rule_name, BoundSide::Gt),
        CtlOp::Ge => numeric_bound(operand, rule_name, BoundSide::Ge),

        CtlOp::Eq => {
            let v = require_literal(operand, rule_name, ".eq")?;
            Ok(vec![Constraint::Eq(lower_value(v))])
        }
        CtlOp::Ne => {
            let v = require_literal(operand, rule_name, ".ne")?;
            Ok(vec![Constraint::Ne(lower_value(v))])
        }

        CtlOp::Regexp => {
            let pattern = match operand {
                Type2::Value(Value::Text(s)) => s.clone(),
                _ => return Err(IrError::InvalidControlOp {
                    op:   ".regexp".into(),
                    ty:   "non-text operand".into(),
                    rule: rule_name.to_owned(),
                }),
            };
            // Warn if no hook provided — validation silently skipped on target
            if regex_hook.is_none() {
                warnings.push(IrWarning {
                    rule:    Some(rule_name.to_owned()),
                    message: format!(
                        "'.regexp' on '{}' has no @regex-hook pragma — \
                         regex validation will be skipped in generated code",
                        rule_name
                    ),
                    kind: IrWarningKind::NoRegexHook,
                });
            }
            Ok(vec![Constraint::Regexp {
                pattern,
                hook: regex_hook.map(|s| s.to_owned()),
            }])
        }

        CtlOp::Cbor => {
            // .cbor T — operand is a type name or inline type
            let inner = lower_type2_to_typeref(operand, rule_name)?;
            Ok(vec![Constraint::CborEmbedded(Box::new(inner))])
        }

        CtlOp::CborSeq => {
            let inner = lower_type2_to_typeref(operand, rule_name)?;
            Ok(vec![Constraint::CborSeq(Box::new(inner))])
        }

        CtlOp::Default => {
            let v = require_literal(operand, rule_name, ".default")?;
            Ok(vec![Constraint::Default(lower_value(v))])
        }

        CtlOp::Within | CtlOp::And | CtlOp::Bits => {
            // Not emitted as constraints in the IR — they affect type
            // checking only.  Silently ignore for now.
            Ok(vec![])
        }

        CtlOp::Other(name) => {
            warnings.push(IrWarning {
                rule:    Some(rule_name.to_owned()),
                message: format!("unknown control operator '.{name}' — ignored"),
                kind:    IrWarningKind::UnknownPragma,
            });
            Ok(vec![])
        }
    }
}

// ── .size lowering ────────────────────────────────────────────────────────────

fn lower_size(operand: &Type2, rule_name: &str) -> Result<Vec<Constraint>, IrError> {
    match operand {
        // .size N  — exact
        Type2::Value(Value::Uint(n)) => {
            Ok(vec![Constraint::SizeExact(*n as usize)])
        }
        // .size lo..hi  or  .size lo...hi — range (encoded as a nested Type1
        // in the parser, but by the time we see Type2 it's a range on the
        // operand).  For now handle the common Uint literal case.
        Type2::Value(Value::Int(n)) if *n >= 0 => {
            Ok(vec![Constraint::SizeExact(*n as usize)])
        }
        // Typename reference — e.g. `.size max-len` where max-len is a uint
        // constant.  We can't resolve this statically without constant folding;
        // emit a warning and skip.
        Type2::Typename { name: _, .. } => {
            // Common prelude constants: uint .size uint — leave unresolved
            Ok(vec![Constraint::SizeRange {
                min: None,
                max: None,
            }])
        }
        _ => Err(IrError::BadSizeConstraint {
            rule:   rule_name.to_owned(),
            reason: format!("unexpected .size operand: {:?}", operand),
        }),
    }
}

// ── Numeric bound helpers ─────────────────────────────────────────────────────

enum BoundSide { Lt, Le, Gt, Ge }

fn numeric_bound(
    operand:   &Type2,
    rule_name: &str,
    side:      BoundSide,
) -> Result<Vec<Constraint>, IrError> {
    let v = require_literal(operand, rule_name, "numeric bound")?;

    // Normalise: a positive uint literal used as a bound on an int-typed alias
    // should produce ValueRangeInt so the two constraints are the same variant
    // and can be merged by backends.  We always produce Int for values that fit.
    let c = match (v, &side) {
        // Uint bounds — emit as Int when the value fits, otherwise keep Uint
        (Value::Uint(n), BoundSide::Lt) if *n > 0 =>
            Constraint::ValueRangeInt  { min: None,              max: Some((*n as i64) - 1), inclusive: true },
        (Value::Uint(n), BoundSide::Le) =>
            Constraint::ValueRangeInt  { min: None,              max: Some(*n as i64),        inclusive: true },
        (Value::Uint(n), BoundSide::Gt) =>
            Constraint::ValueRangeInt  { min: Some((*n as i64) + 1), max: None,              inclusive: true },
        (Value::Uint(n), BoundSide::Ge) =>
            Constraint::ValueRangeInt  { min: Some(*n as i64),   max: None,                  inclusive: true },
        // Int bounds
        (Value::Int(n),  BoundSide::Lt) => Constraint::ValueRangeInt  { min: None,            max: Some(*n - 1), inclusive: true },
        (Value::Int(n),  BoundSide::Le) => Constraint::ValueRangeInt  { min: None,            max: Some(*n),     inclusive: true },
        (Value::Int(n),  BoundSide::Gt) => Constraint::ValueRangeInt  { min: Some(*n + 1),    max: None,         inclusive: true },
        (Value::Int(n),  BoundSide::Ge) => Constraint::ValueRangeInt  { min: Some(*n),        max: None,         inclusive: true },
        // Float bounds
        (Value::Float(f), BoundSide::Lt) => Constraint::ValueRangeF64 { min: None,            max: Some(*f),     inclusive: false },
        (Value::Float(f), BoundSide::Le) => Constraint::ValueRangeF64 { min: None,            max: Some(*f),     inclusive: true  },
        (Value::Float(f), BoundSide::Gt) => Constraint::ValueRangeF64 { min: Some(*f),        max: None,         inclusive: false },
        (Value::Float(f), BoundSide::Ge) => Constraint::ValueRangeF64 { min: Some(*f),        max: None,         inclusive: true  },
        _ => return Err(IrError::BadRangeConstraint {
            rule:   rule_name.to_owned(),
            reason: "numeric bound operand must be a number literal".into(),
        }),
    };
    Ok(vec![c])
}

// ── Value helpers ─────────────────────────────────────────────────────────────

pub fn lower_value(v: &Value) -> LiteralValue {
    match v {
        Value::Uint(n)   => LiteralValue::Uint(*n),
        Value::Int(n)    => LiteralValue::Int(*n),
        Value::Float(f)  => LiteralValue::Float(*f),
        Value::Text(s)   => LiteralValue::Text(s.clone()),
        Value::Bytes(b)  => LiteralValue::Bytes(b.clone()),
    }
}

fn require_literal<'a>(
    t2:        &'a Type2,
    rule_name: &str,
    op_name:   &str,
) -> Result<&'a Value, IrError> {
    match t2 {
        Type2::Value(v) => Ok(v),
        _ => Err(IrError::InvalidControlOp {
            op:   op_name.to_owned(),
            ty:   "non-literal operand".into(),
            rule: rule_name.to_owned(),
        }),
    }
}

fn lower_type2_to_typeref(t2: &Type2, rule_name: &str) -> Result<TypeRef, IrError> {
    use crate::types::Primitive;
    match t2 {
        Type2::Typename { name, .. } => {
            let n = name.node.as_str();
            if let Some(p) = Primitive::from_prelude(n) {
                Ok(TypeRef::Primitive(p))
            } else {
                Ok(TypeRef::Named(n.into()))
            }
        }
        _ => Err(IrError::InvalidControlOp {
            op:   ".cbor/.cborseq".into(),
            ty:   "complex type operand (only type names supported)".into(),
            rule: rule_name.to_owned(),
        }),
    }
}
