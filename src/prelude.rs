/// CDDL standard prelude (RFC 8610, Appendix D).
///
/// These names are always in scope and resolve to primitives without needing
/// a rule definition.  The resolver checks this set before emitting
/// `UndefinedType` errors.

use std::collections::HashSet;

/// Returns the set of all standard prelude type names.
pub fn prelude_names() -> HashSet<&'static str> {
    [
        // Core primitives
        "any", "uint", "int", "bstr", "bytes", "tstr", "text",
        "bool", "nil", "null", "undefined", "true", "false",
        // Float variants
        "float16", "float32", "float64", "float",
        "float16-32",   // float16 | float32
        // Numeric
        "number",       // uint | int | float
        // Byte string aliases
        "b64legacy",    // base64-encoded byte string
        // RFC 8949 well-known tags
        "tdate",        // #6.0(tstr)  — date/time string
        "time",         // #6.1(number) — epoch-based date/time
        "number",       // numeric
        "biguint",      // #6.2(bstr)
        "bigint",       // #6.3(bstr)
        "bigfloat",     // #6.5(array)
        "eb64url",      // #6.21
        "eb64legacy",   // #6.22
        "eb16",         // #6.23
        "encoded-cbor", // #6.24(bstr)
        "uri",          // #6.32(tstr)
        "b64url",       // #6.33(tstr)
        "b64legacy",    // #6.34(tstr)
        "regexp",       // #6.35(tstr)
        "mime-message", // #6.36(tstr)
        "cbor-any",     // #6.55799(any)
    ]
    .iter()
    .copied()
    .collect()
}

/// Returns true if `name` is a CDDL prelude identifier.
pub fn is_prelude(name: &str) -> bool {
    // Fast path for the most common cases
    matches!(
        name,
        "any" | "uint" | "int" | "bstr" | "bytes" | "tstr" | "text"
        | "bool" | "nil" | "null" | "undefined" | "true" | "false"
        | "float16" | "float32" | "float64" | "float"
        | "number" | "b64legacy" | "tdate" | "time"
        | "biguint" | "bigint" | "bigfloat"
        | "eb64url" | "eb64legacy" | "eb16"
        | "encoded-cbor" | "uri" | "b64url" | "regexp"
        | "mime-message" | "cbor-any" | "float16-32"
    )
}
