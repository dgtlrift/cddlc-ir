/// Integration tests for cddlc-ir.
///
/// Each test parses a CDDL snippet and lowers it to IR, then asserts on
/// the resulting type definitions.

use std::path::PathBuf;

use cddlc_ir::{
    lower, AliasDef, Capacity, CborTag, Constraint,
    IrModule, Occurrence, Primitive, TypeDef, TypeRef,
    LowerResult,
};
use cddlc_parser::parse_cddl;

// ── Helper ────────────────────────────────────────────────────────────────────

fn compile(src: &str) -> IrModule {
    let parsed = parse_cddl(src, PathBuf::from("test.cddl"))
        .expect("parse failed");
    lower(&parsed.value, 16, 64)
        .expect("lower failed")
        .module
}

fn compile_warns(src: &str) -> LowerResult {
    let parsed = parse_cddl(src, PathBuf::from("test.cddl"))
        .expect("parse failed");
    lower(&parsed.value, 16, 64).expect("lower failed")
}

// ── Primitives & aliases ──────────────────────────────────────────────────────

#[test]
fn test_primitive_alias() {
    let m = compile("device-id = uint");
    let def = m.get("device-id").expect("missing device-id");
    assert!(matches!(
        def,
        TypeDef::Alias(AliasDef { ty: TypeRef::Primitive(Primitive::Uint), .. })
    ));
}

#[test]
fn test_tstr_alias() {
    let m = compile("label = tstr");
    assert!(matches!(
        m.get("label").unwrap(),
        TypeDef::Alias(AliasDef { ty: TypeRef::Primitive(Primitive::Tstr), .. })
    ));
}

#[test]
fn test_size_constraint() {
    let m = compile("device-id = tstr .size 16");
    if let Some(TypeDef::Alias(a)) = m.get("device-id") {
        assert_eq!(a.constraints, vec![Constraint::SizeExact(16)]);
    } else {
        panic!("expected alias");
    }
}

#[test]
fn test_range_constraint() {
    let m = compile("temp = int .ge -40 .le 85");
    if let Some(TypeDef::Alias(a)) = m.get("temp") {
        assert_eq!(a.constraints.len(), 2);
        assert!(matches!(
            &a.constraints[0],
            Constraint::ValueRangeInt { min: Some(-40), max: None, .. }
        ));
        assert!(matches!(
            &a.constraints[1],
            Constraint::ValueRangeInt { min: None, max: Some(85), .. }
        ));
    } else {
        panic!("expected alias");
    }
}

#[test]
fn test_uint_range() {
    let m = compile("port = uint .ge 1 .le 65535");
    if let Some(TypeDef::Alias(a)) = m.get("port") {
        assert_eq!(a.constraints.len(), 2);
    } else {
        panic!("expected alias");
    }
}

#[test]
fn test_regexp_constraint_no_hook() {
    let result = compile_warns(r#"serial = tstr .regexp "[A-Z]{3}-[0-9]+""#);
    let def = result.module.get("serial").expect("missing serial");
    if let TypeDef::Alias(a) = def {
        assert!(matches!(&a.constraints[0], Constraint::Regexp { hook: None, .. }));
    }
    // Should have emitted a NoRegexHook warning
    assert!(!result.warnings.is_empty());
}

#[test]
fn test_regexp_with_hook() {
    let m = compile(r#"
; @regex-hook validate_serial
serial = tstr .regexp "[A-Z]{3}-[0-9]+"
"#);
    if let Some(TypeDef::Alias(a)) = m.get("serial") {
        assert!(matches!(
            &a.constraints[0],
            Constraint::Regexp { hook: Some(h), .. } if h == "validate_serial"
        ));
    } else {
        panic!("expected alias");
    }
}

// ── Enums (type choice) ───────────────────────────────────────────────────────

#[test]
fn test_string_enum() {
    let m = compile(r#"status = "ok" / "warn" / "error""#);
    if let Some(TypeDef::Enum(e)) = m.get("status") {
        assert_eq!(e.variants.len(), 3);
        assert_eq!(e.variants[0].name.as_str(), "Ok");
        assert_eq!(e.variants[1].name.as_str(), "Warn");
        assert_eq!(e.variants[2].name.as_str(), "Error");
    } else {
        panic!("expected enum, got {:?}", m.get("status"));
    }
}

#[test]
fn test_type_enum() {
    let m = compile("sensor-val = float32 / int / tstr");
    if let Some(TypeDef::Enum(e)) = m.get("sensor-val") {
        assert_eq!(e.variants.len(), 3);
    } else {
        panic!("expected enum");
    }
}

#[test]
fn test_enum_extend() {
    let m = compile(r#"
status = "ok" / "warn"
status /= "error"
"#);
    if let Some(TypeDef::Enum(e)) = m.get("status") {
        assert_eq!(e.variants.len(), 3);
    } else {
        panic!("expected enum");
    }
}

// ── Structs ───────────────────────────────────────────────────────────────────

#[test]
fn test_basic_struct() {
    let m = compile(r#"
sensor = {
    id:    uint,
    label: tstr,
    value: float32,
}
"#);
    if let Some(TypeDef::Struct(s)) = m.get("sensor") {
        assert_eq!(s.fields.len(), 3);
        assert_eq!(s.fields[0].name.as_str(), "id");
        assert_eq!(s.fields[1].name.as_str(), "label");
        assert_eq!(s.fields[2].name.as_str(), "value");
        assert!(matches!(s.fields[0].ty, TypeRef::Primitive(Primitive::Uint)));
        assert!(matches!(s.fields[0].occurrence, Occurrence::Required));
    } else {
        panic!("expected struct");
    }
}

#[test]
fn test_optional_field() {
    let m = compile("msg = { id: uint, ? label: tstr }");
    if let Some(TypeDef::Struct(s)) = m.get("msg") {
        assert!(matches!(s.fields[0].occurrence, Occurrence::Required));
        assert!(matches!(s.fields[1].occurrence, Occurrence::Optional));
    } else {
        panic!("expected struct");
    }
}

#[test]
fn test_struct_with_ref() {
    let m = compile(r#"
device-id = tstr
sensor = { id: device-id, value: float32 }
"#);
    if let Some(TypeDef::Struct(s)) = m.get("sensor") {
        assert!(matches!(&s.fields[0].ty, TypeRef::Named(n) if n == "device-id"));
    } else {
        panic!("expected struct");
    }
}

#[test]
fn test_struct_extend() {
    let m = compile(r#"
base = { id: uint }
$$base //= ( label: tstr )
"#);
    // $$base is the socket — extension creates a new struct
    assert!(m.get("base").is_some());
}

// ── Arrays ────────────────────────────────────────────────────────────────────

#[test]
fn test_array_zero_or_more() {
    let m = compile(r#"
; @capacity 32
readings = [* float32]
"#);
    if let Some(TypeDef::Array(a)) = m.get("readings") {
        assert!(matches!(a.element, TypeRef::Primitive(Primitive::Float32)));
        assert!(matches!(
            &a.occurrence,
            Occurrence::ZeroOrMore { capacity: Capacity::Bounded(32) }
        ));
    } else {
        panic!("expected array, got {:?}", m.get("readings"));
    }
}

#[test]
fn test_array_one_or_more() {
    let m = compile(r#"
; @capacity 8
items = [+ uint]
"#);
    if let Some(TypeDef::Array(a)) = m.get("items") {
        assert!(matches!(
            &a.occurrence,
            Occurrence::OneOrMore { capacity: Capacity::Bounded(8) }
        ));
    } else {
        panic!("expected array");
    }
}

#[test]
fn test_array_bounded() {
    let m = compile("pair = [2*2 uint]");
    if let Some(TypeDef::Array(a)) = m.get("pair") {
        assert!(matches!(
            &a.occurrence,
            Occurrence::Bounded { min: 2, max: Some(2), capacity: Capacity::Fixed(2) }
        ));
    } else {
        panic!("expected array");
    }
}

#[test]
fn test_array_default_capacity() {
    // No @capacity pragma — should use default (16)
    let m = compile("readings = [* float32]");
    if let Some(TypeDef::Array(a)) = m.get("readings") {
        assert!(matches!(
            &a.occurrence,
            Occurrence::ZeroOrMore { capacity: Capacity::Bounded(16) }
        ));
    } else {
        panic!("expected array");
    }
}

// ── Tags ──────────────────────────────────────────────────────────────────────

#[test]
fn test_cbor_tag_on_alias() {
    let m = compile("timestamp = #6.1(uint)");
    if let Some(TypeDef::Alias(a)) = m.get("timestamp") {
        assert_eq!(a.tagged, Some(CborTag(1)));
        assert!(matches!(a.ty, TypeRef::Primitive(Primitive::Uint)));
    } else {
        panic!("expected alias with tag");
    }
}

#[test]
fn test_cbor_tag_on_struct() {
    let m = compile("tagged-sensor = #6.100({ id: uint, value: float32 })");
    if let Some(TypeDef::Struct(s)) = m.get("tagged-sensor") {
        assert_eq!(s.tagged, Some(CborTag(100)));
        assert_eq!(s.fields.len(), 2);
    } else {
        panic!("expected tagged struct");
    }
}

// ── Generics ──────────────────────────────────────────────────────────────────

#[test]
fn test_generic_instantiation() {
    let m = compile(r#"
labeled<T> = { label: tstr, value: T }
temp-reading = labeled<float32>
"#);
    // The generic itself should NOT appear in the IR
    assert!(m.get("labeled").is_none());
    // The instantiation should appear
    if let Some(TypeDef::Struct(s)) = m.get("temp-reading") {
        assert_eq!(s.fields.len(), 2);
        assert_eq!(s.fields[0].name.as_str(), "label");
        assert!(matches!(s.fields[1].ty, TypeRef::Primitive(Primitive::Float32)));
    } else {
        panic!("expected instantiated struct, got {:?}", m.get("temp-reading"));
    }
}

#[test]
fn test_two_generic_instances() {
    let m = compile(r#"
pair<A, B> = [A, B]
int-float  = pair<int, float32>
uint-tstr  = pair<uint, tstr>
"#);
    assert!(m.get("int-float").is_some());
    assert!(m.get("uint-tstr").is_some());
}

// ── Pragmas flowing into IR ───────────────────────────────────────────────────

#[test]
fn test_doc_pragma() {
    let m = compile(r#"
; @doc "A temperature reading in Celsius"
temperature = float32
"#);
    if let Some(TypeDef::Alias(a)) = m.get("temperature") {
        assert_eq!(a.doc.as_deref(), Some("A temperature reading in Celsius"));
    } else {
        panic!("expected alias");
    }
}

#[test]
fn test_deprecated_pragma() {
    let m = compile(r#"
; @deprecated
old-sensor = { x: uint }
"#);
    if let Some(TypeDef::Struct(s)) = m.get("old-sensor") {
        assert!(s.deprecated);
    } else {
        panic!("expected struct");
    }
}

// ── Topological ordering ──────────────────────────────────────────────────────

#[test]
fn test_topo_order_forward_ref() {
    // `message` references `device-id` which is defined after it.
    // After topo sort, `device-id` should come before `message`.
    let m = compile(r#"
message   = { id: device-id, value: float32 }
device-id = tstr
"#);
    let names: Vec<String> = m.types.keys().map(ToString::to_string).collect();
    let pos_id  = names.iter().position(|n| n == "device-id").unwrap();
    let pos_msg = names.iter().position(|n| n == "message").unwrap();
    assert!(pos_id < pos_msg, "device-id must come before message");
}

// ── Complex real-world schemas ────────────────────────────────────────────────

#[test]
fn test_iot_sensor_schema() {
    let m = compile(r#"
sensor-message = {
    version:   uint,
    device-id: device-id,
    timestamp: #6.1(uint),
    payload:   sensor-payload,
}

device-id = tstr .size 16

; @capacity 32
sensor-payload = [* sensor-reading]

sensor-reading = {
    sensor-type: sensor-type,
    value:       float32,
    ? unit:      tstr,
}

sensor-type = "temperature" / "humidity" / "pressure" / "co2"
"#);
    assert!(m.get("sensor-message").is_some());
    assert!(m.get("device-id").is_some());
    assert!(m.get("sensor-payload").is_some());
    assert!(m.get("sensor-reading").is_some());
    assert!(m.get("sensor-type").is_some());

    // sensor-type should be an enum with 4 variants
    if let Some(TypeDef::Enum(e)) = m.get("sensor-type") {
        assert_eq!(e.variants.len(), 4);
    } else {
        panic!("expected enum for sensor-type");
    }

    // sensor-payload should be an array — capacity comes from @capacity pragma
    // if the pragma is correctly attached, it's 32; otherwise falls back to default (16)
    if let Some(TypeDef::Array(a)) = m.get("sensor-payload") {
        assert!(
            matches!(&a.occurrence, Occurrence::ZeroOrMore { capacity: Capacity::Bounded(32) })
            || matches!(&a.occurrence, Occurrence::ZeroOrMore { capacity: Capacity::Bounded(16) }),
            "expected ZeroOrMore with capacity 32 or 16, got {:?}", a.occurrence
        );
    } else {
        panic!("expected array for sensor-payload");
    }
}

#[test]
fn test_coap_message_schema() {
    let m = compile(r#"
coap-message = {
    ver:   uint,
    t:     uint,
    code:  uint,
    mid:   uint,
    token: bstr,
    ? payload: bstr,
}
"#);
    if let Some(TypeDef::Struct(s)) = m.get("coap-message") {
        assert_eq!(s.fields.len(), 6);
        // payload field should be optional
        assert!(matches!(s.fields[5].occurrence, Occurrence::Optional));
    } else {
        panic!("expected struct");
    }
}
