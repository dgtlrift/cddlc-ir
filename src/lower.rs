/// AST → IR lowering.
///
/// Walks the parser [`CddlModule`] and produces a resolved [`IrModule`].
///
/// Stages (in order):
/// 1. Collect all rule names into a resolution table (forward-decl pass)
/// 2. Collect generic rule templates
/// 3. Lower each non-generic rule:
///    a. Resolve type references (prelude, user rules, generics)
///    b. Lower control operators → Constraints
///    c. Propagate pragma metadata (capacity, doc, deprecated, regex-hook)
///    d. Classify each rule as Struct / Enum / Array / Map / Alias
/// 4. Topological sort
/// 5. Emit LowerResult

use std::collections::{HashMap, HashSet};

use indexmap::IndexMap;
use smol_str::SmolStr;

use cddlc_parser::{
    CddlModule, GroupChoice, MemberKind, Occurrence as AstOcc,
    Pragma, PragmaValue, Rule, RuleKind, Spanned, Type, Type1, Type2,
    Value,
};

use crate::error::{IrError, IrWarning, LowerResult};
use crate::lower_constraints::{lower_type_op, lower_value};
use crate::prelude::is_prelude;
use crate::types::{
    AliasDef, ArrayDef, Capacity, CborTag, Constraint, EnumDef, EnumVariant,
    FieldDef, FieldKey, IrModule, LiteralValue, Occurrence,
    Primitive, StructDef, TypeDef, TypeRef,
};

// ── Public entry point ────────────────────────────────────────────────────────

/// Lower a parsed [`CddlModule`] into a resolved [`IrModule`].
///
/// `default_capacity` and `default_str_capacity` come from CLI flags;
/// they are used when a rule has no `@capacity`/`@str-capacity` pragma.
pub fn lower(
    module:               &CddlModule,
    default_capacity:     usize,
    default_str_capacity: usize,
) -> Result<LowerResult, IrError> {
    let mut ctx = LowerCtx::new(module, default_capacity, default_str_capacity);
    ctx.run()
}

// ── Lowering context ──────────────────────────────────────────────────────────

struct LowerCtx<'m> {
    module:                &'m CddlModule,
    default_capacity:      usize,
    _default_str_capacity: usize,

    /// All rule names defined in this module (for forward-ref resolution).
    known_rules: HashSet<SmolStr>,

    /// Generic rule templates: name → (params, rule).
    generics: HashMap<SmolStr, (Vec<SmolStr>, &'m Rule)>,

    /// Output accumulator — in definition order (topo sort applied later).
    output: IndexMap<SmolStr, TypeDef>,

    warnings: Vec<IrWarning>,
}

impl<'m> LowerCtx<'m> {
    fn new(
        module:               &'m CddlModule,
        default_capacity:     usize,
        default_str_capacity: usize,
    ) -> Self {
        Self {
            module,
            default_capacity,
            _default_str_capacity: default_str_capacity,
            known_rules:  HashSet::new(),
            generics:     HashMap::new(),
            output:       IndexMap::new(),
            warnings:     Vec::new(),
        }
    }

    fn run(&mut self) -> Result<LowerResult, IrError> {
        // Pass 1: collect all defined names
        for sr in &self.module.rules {
            self.known_rules.insert(sr.node.name.node.0.clone());
        }

        // Pass 2: collect generic templates
        for sr in &self.module.rules {
            let rule = &sr.node;
            if !rule.generic_params.is_empty() {
                let params: Vec<SmolStr> = rule.generic_params
                    .iter()
                    .map(|p| p.node.0.clone())
                    .collect();
                self.generics.insert(rule.name.node.0.clone(), (params, rule));
            }
        }

        // Pass 3: lower non-generic rules
        for sr in &self.module.rules {
            let rule = &sr.node;
            if rule.generic_params.is_empty() {
                self.lower_rule(rule)?;
            }
        }

        // Pass 4: topological sort
        let sorted = topo_sort(&self.output)?;

        let mut ir = IrModule::new(
            self.module.source_path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "module".into())
        );
        for name in sorted {
            if let Some(def) = self.output.swap_remove(&name) {
                ir.insert(name, def);
            }
        }

        Ok(LowerResult {
            module:   ir,
            warnings: std::mem::take(&mut self.warnings),
        })
    }

    // ── Rule dispatch ─────────────────────────────────────────────────────────

    fn lower_rule(&mut self, rule: &'m Rule) -> Result<(), IrError> {
        let name: SmolStr = rule.name.node.0.clone();

        // Extract pragma metadata
        let meta = PragmaMeta::from_pragmas(&rule.pragmas);

        match &rule.kind {
            RuleKind::Type { ty, extend: false } => {
                let def = self.lower_type_rule(&name, &ty.node, &meta)?;
                self.output.insert(name, def);
            }
            RuleKind::Type { ty, extend: true } => {
                self.lower_type_extend(&name, &ty.node)?;
            }
            RuleKind::Group { group, extend: false } => {
                // A group rule with `(` — treat as struct
                let def = self.lower_group_as_struct(&name, &group.node.choices, &meta)?;
                self.output.insert(name, def);
            }
            RuleKind::Group { group, extend: true } => {
                self.lower_group_extend(&name, &group.node.choices)?;
            }
        }
        Ok(())
    }

    // ── Type rule lowering ────────────────────────────────────────────────────

    fn lower_type_rule(
        &mut self,
        name: &str,
        ty:   &Type,
        meta: &PragmaMeta,
    ) -> Result<TypeDef, IrError> {
        // A type with multiple choices → Enum
        if ty.choices.len() > 1 {
            return self.lower_as_enum(name, ty, meta);
        }

        let t1 = &ty.choices[0].node;

        // Classify the base type2
        match &t1.base.node {
            // Map → Struct
            Type2::Map(group) => {
                self.lower_map_as_struct(name, &group.node.choices, meta)
            }

            // Array → ArrayDef or StructDef (tuple-style)
            Type2::Array(group) => {
                self.lower_array_type(name, &group.node.choices, meta)
            }

            // Tagged type: #6.NN(inner)
            Type2::Tagged { tag, inner } => {
                let cbor_tag = CborTag(*tag);
                let mut inner_meta = meta.clone();
                inner_meta.tag = Some(cbor_tag);
                self.lower_tagged_type(name, &inner.node, &inner_meta)
            }

            // Typename reference — alias or forward ref
            Type2::Typename { name: ref_name, args } => {
                if args.is_empty() {
                    self.lower_alias(name, ref_name.node.as_str(), t1, meta)
                } else {
                    // Generic instantiation
                    self.lower_generic_instance(
                        name,
                        ref_name.node.as_str(),
                        args,
                        meta,
                    )
                }
            }

            // Value literal used as a type → single-variant enum / alias
            Type2::Value(v) => {
                let lit = lower_value(v);
                let def = AliasDef {
                    name:        name.into(),
                    ty:          literal_to_typeref(&lit),
                    constraints: vec![Constraint::Eq(lit)],
                    tagged:      meta.tag,
                    doc:         meta.doc.clone(),
                    deprecated:  meta.deprecated,
                };
                Ok(TypeDef::Alias(def))
            }

            // Any / MajorType → alias to Any
            Type2::Any | Type2::MajorType(_) => {
                Ok(TypeDef::Alias(AliasDef {
                    name:        name.into(),
                    ty:          TypeRef::Primitive(Primitive::Any),
                    constraints: vec![],
                    tagged:      meta.tag,
                    doc:         meta.doc.clone(),
                    deprecated:  meta.deprecated,
                }))
            }

            Type2::Unwrap(ident) => {
                Ok(TypeDef::Alias(AliasDef {
                    name:        name.into(),
                    ty:          TypeRef::Named(ident.node.0.clone()),
                    constraints: vec![],
                    tagged:      meta.tag,
                    doc:         meta.doc.clone(),
                    deprecated:  meta.deprecated,
                }))
            }

            // Group / Paren — treat same as map
            Type2::Group(group) => {
                self.lower_map_as_struct(name, &group.node.choices, meta)
            }
        }
    }

    // ── Enum (type choice) ────────────────────────────────────────────────────

    fn lower_as_enum(
        &mut self,
        name: &str,
        ty:   &Type,
        meta: &PragmaMeta,
    ) -> Result<TypeDef, IrError> {
        let mut variants = Vec::new();
        for (i, choice) in ty.choices.iter().enumerate() {
            let t2 = &choice.node.base.node;
            let (var_name, type_ref) = self.classify_enum_variant(t2, i, name)?;
            variants.push(EnumVariant { name: var_name, ty: type_ref });
        }
        Ok(TypeDef::Enum(EnumDef {
            name:       name.into(),
            variants,
            tagged:     meta.tag,
            doc:        meta.doc.clone(),
            deprecated: meta.deprecated,
        }))
    }

    fn classify_enum_variant(
        &self,
        t2:        &Type2,
        index:     usize,
        rule_name: &str,
    ) -> Result<(SmolStr, TypeRef), IrError> {
        match t2 {
            Type2::Value(Value::Text(s)) => {
                let var_name = pascal_case(s);
                Ok((var_name.into(), TypeRef::Primitive(Primitive::Tstr)))
            }
            Type2::Value(Value::Uint(n)) => {
                Ok((format!("Variant{n}").into(), TypeRef::Primitive(Primitive::Uint)))
            }
            Type2::Value(Value::Int(n)) => {
                Ok((format!("Variant{n}").into(), TypeRef::Primitive(Primitive::Int)))
            }
            Type2::Typename { name, .. } => {
                let n = name.node.as_str();
                let tr = self.resolve_typeref(n, rule_name)?;
                Ok((pascal_case(n).into(), tr))
            }
            _ => {
                Ok((format!("Variant{index}").into(),
                    TypeRef::Primitive(Primitive::Any)))
            }
        }
    }

    // ── Struct (named map) ────────────────────────────────────────────────────

    fn lower_map_as_struct(
        &mut self,
        name:    &str,
        choices: &[GroupChoice],
        meta:    &PragmaMeta,
    ) -> Result<TypeDef, IrError> {
        // For now we take the first (and usually only) group choice.
        // Group choices (//= extensions) are handled by lower_group_extend.
        let fields = choices.iter()
            .flat_map(|gc| gc.entries.iter())
            .map(|e| self.lower_member_entry(e, name))
            .collect::<Result<Vec<_>, _>>()?;

        Ok(TypeDef::Struct(StructDef {
            name:       name.into(),
            fields,
            tagged:     meta.tag,
            dcbor:      false,
            doc:        meta.doc.clone(),
            deprecated: meta.deprecated,
        }))
    }

    fn lower_group_as_struct(
        &mut self,
        name:    &str,
        choices: &[GroupChoice],
        meta:    &PragmaMeta,
    ) -> Result<TypeDef, IrError> {
        let fields = choices.iter()
            .flat_map(|gc| gc.entries.iter())
            .map(|e| self.lower_member_entry(e, name))
            .collect::<Result<Vec<_>, _>>()?;

        Ok(TypeDef::Struct(StructDef {
            name:       name.into(),
            fields,
            tagged:     meta.tag,
            dcbor:      false,
            doc:        meta.doc.clone(),
            deprecated: meta.deprecated,
        }))
    }

    fn lower_member_entry(
        &mut self,
        entry:     &Spanned<cddlc_parser::MemberEntry>,
        rule_name: &str,
    ) -> Result<FieldDef, IrError> {
        let me = &entry.node;

        // Occurrence
        let occurrence = self.lower_occurrence(
            me.occurrence.as_ref().map(|o| &o.node),
            rule_name,
            &PragmaMeta::from_pragmas(&me.pragmas),
        );

        match &me.kind {
            MemberKind::KeyValue { key, value, .. } => {
                let field_name: SmolStr = match &key.node {
                    cddlc_parser::MemberKey::Bareword(id) => id.0.clone(),
                    cddlc_parser::MemberKey::Value(Value::Text(s)) => s.as_str().into(),
                    cddlc_parser::MemberKey::Value(Value::Uint(n)) => format!("field_{n}").into(),
                    cddlc_parser::MemberKey::Value(Value::Int(n))  => format!("field_{n}").into(),
                    _ => "unknown_key".into(),
                };

                let field_key = match &key.node {
                    cddlc_parser::MemberKey::Bareword(id) => FieldKey::Text(id.0.clone()),
                    cddlc_parser::MemberKey::Value(Value::Text(s)) => FieldKey::Text(s.as_str().into()),
                    cddlc_parser::MemberKey::Value(Value::Uint(n)) => FieldKey::Uint(*n),
                    cddlc_parser::MemberKey::Value(Value::Int(n))  => FieldKey::Int(*n),
                    _ => FieldKey::Text(field_name.clone()),
                };

                let (ty, constraints) = self.lower_type_to_ref(
                    &value.node, rule_name
                )?;

                Ok(FieldDef {
                    name:        field_name,
                    key:         field_key,
                    ty,
                    occurrence,
                    constraints,
                    default:     None,
                    doc:         None,
                    deprecated:  false,
                })
            }

            MemberKind::Type(ty) => {
                // Bare type embedded in group — used for group references
                let (tr, constraints) = self.lower_type_to_ref(&ty.node, rule_name)?;
                let field_name: SmolStr = match &tr {
                    TypeRef::Named(n) => n.clone(),
                    TypeRef::Primitive(p) => format!("{p:?}").to_lowercase().into(),
                    _ => "value".into(),
                };
                Ok(FieldDef {
                    name:        field_name,
                    key:         FieldKey::Text("value".into()),
                    ty:          tr,
                    occurrence,
                    constraints,
                    default:     None,
                    doc:         None,
                    deprecated:  false,
                })
            }
        }
    }

    // ── Array type ────────────────────────────────────────────────────────────

    fn lower_array_type(
        &mut self,
        name:    &str,
        choices: &[GroupChoice],
        meta:    &PragmaMeta,
    ) -> Result<TypeDef, IrError> {
        // Detect homogeneous array: single entry with occurrence
        let entries: Vec<_> = choices.iter()
            .flat_map(|gc| gc.entries.iter())
            .collect();

        if entries.len() == 1 {
            let entry = &entries[0].node;
            let occ = self.lower_occurrence(
                entry.occurrence.as_ref().map(|o| &o.node),
                name,
                &PragmaMeta::from_pragmas_with_capacity(
                    &entry.pragmas,
                    meta.capacity,
                    meta.str_capacity,
                ),
            );

            let ty = match &entry.kind {
                MemberKind::Type(t) => {
                    let (tr, _) = self.lower_type_to_ref(&t.node, name)?;
                    tr
                }
                MemberKind::KeyValue { value, .. } => {
                    let (tr, _) = self.lower_type_to_ref(&value.node, name)?;
                    tr
                }
            };

            return Ok(TypeDef::Array(ArrayDef {
                name:       name.into(),
                element:    ty,
                occurrence: occ,
                tagged:     meta.tag,
                doc:        meta.doc.clone(),
                deprecated: meta.deprecated,
            }));
        }

        // Multi-entry array → struct with positional fields
        self.lower_map_as_struct(name, choices, meta)
    }

    // ── Tagged type ───────────────────────────────────────────────────────────

    fn lower_tagged_type(
        &mut self,
        name: &str,
        ty:   &cddlc_parser::Type,
        meta: &PragmaMeta,
    ) -> Result<TypeDef, IrError> {
        // Recurse — the tag is already in meta
        self.lower_type_rule(name, ty, meta)
    }

    // ── Alias ─────────────────────────────────────────────────────────────────

    fn lower_alias(
        &mut self,
        name:     &str,
        ref_name: &str,
        t1:       &Type1,
        meta:     &PragmaMeta,
    ) -> Result<TypeDef, IrError> {
        let tr = self.resolve_typeref(ref_name, name)?;

        // Gather constraints from control operators
        let mut constraints: Vec<Constraint> = Vec::new();
        for op in &t1.ops {
            let cs = lower_type_op(
                op, name, meta.regex_hook.as_deref(), &mut self.warnings,
            )?;
            constraints.extend(cs);
        }

        Ok(TypeDef::Alias(AliasDef {
            name:        name.into(),
            ty:          tr,
            constraints,
            tagged:      meta.tag,
            doc:         meta.doc.clone(),
            deprecated:  meta.deprecated,
        }))
    }

    // ── Generic instantiation ─────────────────────────────────────────────────

    fn lower_generic_instance(
        &mut self,
        instance_name: &str,
        generic_name:  &str,
        args:          &[Spanned<Type>],
        meta:          &PragmaMeta,
    ) -> Result<TypeDef, IrError> {
        let (params, template_rule) = self
            .generics
            .get(generic_name)
            .map(|(p, r)| (p.clone(), *r))
            .ok_or_else(|| IrError::NotGeneric { name: generic_name.to_owned() })?;

        if params.len() != args.len() {
            return Err(IrError::GenericArgCount {
                name:     generic_name.to_owned(),
                expected: params.len(),
                got:      args.len(),
            });
        }

        // Build substitution map: param name → concrete TypeRef
        let mut subst: HashMap<SmolStr, TypeRef> = HashMap::new();
        for (param, arg) in params.iter().zip(args.iter()) {
            let tr = self.lower_type_single(&arg.node, instance_name)?;
            subst.insert(param.clone(), tr);
        }

        // Substitute into the template rule and lower it under the instance name
        self.lower_rule_with_subst(instance_name, template_rule, &subst, meta)
    }

    fn lower_rule_with_subst(
        &mut self,
        name:     &str,
        rule:     &Rule,
        subst:    &HashMap<SmolStr, TypeRef>,
        meta:     &PragmaMeta,
    ) -> Result<TypeDef, IrError> {
        match &rule.kind {
            RuleKind::Type { ty, .. } => {
                let substituted = self.substitute_type(&ty.node, subst);
                self.lower_type_rule(name, &substituted, meta)
            }
            RuleKind::Group { group, .. } => {
                // For group rules with substitution, lower as struct
                // (substitution in group entries is handled field by field)
                let fields: Result<Vec<FieldDef>, _> = group.node.choices.iter()
                    .flat_map(|gc| gc.entries.iter())
                    .map(|e| self.lower_member_entry_with_subst(e, name, subst))
                    .collect();

                Ok(TypeDef::Struct(StructDef {
                    name:       name.into(),
                    fields:     fields?,
                    tagged:     meta.tag,
                    dcbor:      false,
                    doc:        meta.doc.clone(),
                    deprecated: meta.deprecated,
                }))
            }
        }
    }

    fn lower_member_entry_with_subst(
        &mut self,
        entry:  &Spanned<cddlc_parser::MemberEntry>,
        rule_name: &str,
        subst:  &HashMap<SmolStr, TypeRef>,
    ) -> Result<FieldDef, IrError> {
        let me = &entry.node;
        let occurrence = self.lower_occurrence(
            me.occurrence.as_ref().map(|o| &o.node),
            rule_name,
            &PragmaMeta::default(),
        );

        match &me.kind {
            MemberKind::KeyValue { key, value, .. } => {
                let field_name: SmolStr = match &key.node {
                    cddlc_parser::MemberKey::Bareword(id) => id.0.clone(),
                    cddlc_parser::MemberKey::Value(Value::Text(s)) => s.as_str().into(),
                    _ => "field".into(),
                };
                let field_key = match &key.node {
                    cddlc_parser::MemberKey::Bareword(id) => FieldKey::Text(id.0.clone()),
                    cddlc_parser::MemberKey::Value(Value::Text(s)) => FieldKey::Text(s.as_str().into()),
                    cddlc_parser::MemberKey::Value(Value::Uint(n)) => FieldKey::Uint(*n),
                    cddlc_parser::MemberKey::Value(Value::Int(n))  => FieldKey::Int(*n),
                    _ => FieldKey::Text(field_name.clone()),
                };

                // Substitute type parameters
                let subst_type = self.substitute_type(&value.node, subst);
                let (ty, constraints) = self.lower_type_to_ref(&subst_type, rule_name)?;

                Ok(FieldDef {
                    name: field_name, key: field_key, ty,
                    occurrence, constraints,
                    default: None, doc: None, deprecated: false,
                })
            }
            MemberKind::Type(ty_node) => {
                let subst_type = self.substitute_type(&ty_node.node, subst);
                let (tr, constraints) = self.lower_type_to_ref(&subst_type, rule_name)?;
                let field_name: SmolStr = match &tr {
                    TypeRef::Named(n) => n.clone(),
                    _ => "value".into(),
                };
                Ok(FieldDef {
                    name: field_name.clone(),
                    key: FieldKey::Text(field_name),
                    ty: tr, occurrence, constraints,
                    default: None, doc: None, deprecated: false,
                })
            }
        }
    }

    // ── Type substitution (for generics) ──────────────────────────────────────

    fn substitute_type(&self, ty: &Type, subst: &HashMap<SmolStr, TypeRef>) -> Type {
        Type {
            choices: ty.choices.iter().map(|c| {
                Spanned::new(
                    cddlc_parser::Type1 {
                        base: self.substitute_type2(&c.node.base, subst),
                        ops:  c.node.ops.clone(),
                    },
                    c.span,
                )
            }).collect()
        }
    }

    fn substitute_type2(
        &self,
        t2:    &Spanned<Type2>,
        subst: &HashMap<SmolStr, TypeRef>,
    ) -> Spanned<Type2> {
        match &t2.node {
            // Direct type parameter reference — substitute it
            Type2::Typename { name, args } if args.is_empty() => {
                if let Some(replacement) = subst.get(name.node.as_str()) {
                    if let TypeRef::Primitive(p) = replacement {
                        let new_name = primitive_to_prelude_name(p);
                        return Spanned::new(
                            Type2::Typename {
                                name: Spanned::new(
                                    cddlc_parser::Ident::new(new_name),
                                    name.span,
                                ),
                                args: vec![],
                            },
                            t2.span,
                        );
                    }
                    if let TypeRef::Named(n) = replacement {
                        return Spanned::new(
                            Type2::Typename {
                                name: Spanned::new(
                                    cddlc_parser::Ident::new(n.as_str()),
                                    name.span,
                                ),
                                args: vec![],
                            },
                            t2.span,
                        );
                    }
                }
                t2.clone()
            }

            // Recurse into map/array/group — substitute through member entries
            Type2::Map(group) => {
                let new_group = self.substitute_group(&group.node, subst);
                Spanned::new(Type2::Map(Spanned::new(new_group, group.span)), t2.span)
            }
            Type2::Array(group) => {
                let new_group = self.substitute_group(&group.node, subst);
                Spanned::new(Type2::Array(Spanned::new(new_group, group.span)), t2.span)
            }
            Type2::Group(group) => {
                let new_group = self.substitute_group(&group.node, subst);
                Spanned::new(Type2::Group(Spanned::new(new_group, group.span)), t2.span)
            }

            _ => t2.clone(),
        }
    }

    fn substitute_group(
        &self,
        group: &cddlc_parser::GroupEntry,
        subst: &HashMap<SmolStr, TypeRef>,
    ) -> cddlc_parser::GroupEntry {
        use cddlc_parser::{GroupChoice, GroupEntry, MemberEntry, MemberKind};

        let choices = group.choices.iter().map(|gc| {
            let entries = gc.entries.iter().map(|e| {
                let me = &e.node;
                let new_kind = match &me.kind {
                    MemberKind::KeyValue { key, value, cut } => {
                        let new_value = Spanned::new(
                            self.substitute_type(&value.node, subst),
                            value.span,
                        );
                        MemberKind::KeyValue {
                            key:   key.clone(),
                            value: new_value,
                            cut:   *cut,
                        }
                    }
                    MemberKind::Type(ty) => {
                        MemberKind::Type(Spanned::new(
                            self.substitute_type(&ty.node, subst),
                            ty.span,
                        ))
                    }
                };
                Spanned::new(
                    MemberEntry { kind: new_kind, occurrence: me.occurrence.clone(), pragmas: me.pragmas.clone() },
                    e.span,
                )
            }).collect();
            GroupChoice { entries }
        }).collect();
        GroupEntry { choices }
    }

    // ── Extend rules ──────────────────────────────────────────────────────────

    fn lower_type_extend(&mut self, name: &str, ty: &Type) -> Result<(), IrError> {
        // `/=` extension: add new variants to an existing enum
        let new_variants = ty.choices.iter()
            .enumerate()
            .map(|(i, c)| self.classify_enum_variant(&c.node.base.node, i, name))
            .collect::<Result<Vec<_>, _>>()?;

        match self.output.get_mut(name) {
            Some(TypeDef::Enum(e)) => {
                for (vname, vty) in new_variants {
                    e.variants.push(EnumVariant { name: vname, ty: vty });
                }
                Ok(())
            }
            Some(_) => Err(IrError::IncompatibleExtension {
                name:   name.to_owned(),
                reason: "/= extension requires the base rule to be a type choice".into(),
            }),
            None => {
                // Base rule not yet defined — create it as an enum
                let variants = new_variants.into_iter()
                    .map(|(n, t)| EnumVariant { name: n, ty: t })
                    .collect();
                self.output.insert(name.into(), TypeDef::Enum(EnumDef {
                    name:       name.into(),
                    variants,
                    tagged:     None,
                    doc:        None,
                    deprecated: false,
                }));
                Ok(())
            }
        }
    }

    fn lower_group_extend(&mut self, name: &str, choices: &[GroupChoice]) -> Result<(), IrError> {
        // `//=` extension: add new fields to an existing struct
        let new_fields: Vec<FieldDef> = choices.iter()
            .flat_map(|gc| gc.entries.iter())
            .map(|e| self.lower_member_entry(e, name))
            .collect::<Result<Vec<_>, _>>()?;

        match self.output.get_mut(name) {
            Some(TypeDef::Struct(s)) => {
                s.fields.extend(new_fields);
                Ok(())
            }
            Some(_) => Err(IrError::IncompatibleExtension {
                name:   name.to_owned(),
                reason: "//= extension requires the base rule to be a map/struct".into(),
            }),
            None => {
                self.output.insert(name.into(), TypeDef::Struct(StructDef {
                    name:       name.into(),
                    fields:     new_fields,
                    tagged:     None,
                    dcbor:      false,
                    doc:        None,
                    deprecated: false,
                }));
                Ok(())
            }
        }
    }

    // ── Type reference resolution ─────────────────────────────────────────────

    /// Resolve a type name to a TypeRef.
    fn resolve_typeref(&self, name: &str, in_rule: &str) -> Result<TypeRef, IrError> {
        if let Some(p) = Primitive::from_prelude(name) {
            return Ok(TypeRef::Primitive(p));
        }
        if is_prelude(name) {
            return Ok(TypeRef::Primitive(Primitive::Any)); // tagged prelude types
        }
        if self.known_rules.contains(name) {
            return Ok(TypeRef::Named(name.into()));
        }
        Err(IrError::UndefinedType {
            name:    name.to_owned(),
            in_rule: in_rule.to_owned(),
        })
    }

    /// Lower a full `Type` expression to a single TypeRef + constraints.
    fn lower_type_to_ref(
        &mut self,
        ty:        &Type,
        rule_name: &str,
    ) -> Result<(TypeRef, Vec<Constraint>), IrError> {
        if ty.choices.len() > 1 {
            // Inline choice — produce TypeRef::Choice
            let refs: Vec<TypeRef> = ty.choices.iter()
                .map(|c| self.lower_type2_ref(&c.node.base.node, rule_name))
                .collect::<Result<Vec<TypeRef>, IrError>>()?;
            return Ok((TypeRef::Choice(refs), vec![]));
        }

        let t1 = &ty.choices[0].node;
        let tr = self.lower_type2_ref(&t1.base.node, rule_name)?;

        let mut constraints = Vec::new();
        for op in &t1.ops {
            let cs = lower_type_op(op, rule_name, None, &mut self.warnings)?;
            constraints.extend(cs);
        }

        Ok((tr, constraints))
    }

    fn lower_type2_ref(&mut self, t2: &Type2, rule_name: &str) -> Result<TypeRef, IrError> {
        match t2 {
            Type2::Typename { name, args } if args.is_empty() => {
                self.resolve_typeref(name.node.as_str(), rule_name)
            }
            Type2::Typename { name, args } => {
                // Generic application — instantiate inline
                let instance_name: SmolStr = format!(
                    "{}_{}",
                    name.node.as_str(),
                    args.iter()
                        .map(|a| type_to_name(&a.node))
                        .collect::<Vec<_>>()
                        .join("_")
                ).into();

                if !self.output.contains_key(&instance_name)
                    && !self.known_rules.contains(&instance_name)
                {
                    let meta = PragmaMeta::default();
                    let def = self.lower_generic_instance(
                        &instance_name, name.node.as_str(), args, &meta,
                    )?;
                    self.output.insert(instance_name.clone(), def);
                    self.known_rules.insert(instance_name.clone());
                }
                Ok(TypeRef::Named(instance_name))
            }
            Type2::Value(v) => Ok(literal_to_typeref(&lower_value(v))),
            Type2::Tagged { tag, inner } => {
                let inner_tr = self.lower_type_single(&inner.node, rule_name)?;
                Ok(TypeRef::Tagged(CborTag(*tag), Box::new(inner_tr)))
            }
            Type2::Any | Type2::MajorType(_) => Ok(TypeRef::Primitive(Primitive::Any)),
            Type2::Unwrap(id) => self.resolve_typeref(id.node.as_str(), rule_name),
            Type2::Map(_) | Type2::Array(_) | Type2::Group(_) => {
                // Inline complex type — generate a synthetic named type
                Ok(TypeRef::Primitive(Primitive::Any))
            }
        }
    }

    fn lower_type_single(&mut self, ty: &Type, rule_name: &str) -> Result<TypeRef, IrError> {
        let (tr, _) = self.lower_type_to_ref(ty, rule_name)?;
        Ok(tr)
    }

    // ── Occurrence lowering ───────────────────────────────────────────────────

    fn lower_occurrence(
        &self,
        occ:        Option<&AstOcc>,
        _rule_name: &str,
        meta:       &PragmaMeta,
    ) -> Occurrence {
        let cap = |default: usize| -> Capacity {
            match meta.capacity {
                Some(n) => Capacity::Bounded(n),
                None    => {
                    // Warn about default capacity usage
                    Capacity::Bounded(default)
                }
            }
        };

        match occ {
            None                    => Occurrence::Required,
            Some(AstOcc::Optional)  => Occurrence::Optional,
            Some(AstOcc::ZeroOrMore) => Occurrence::ZeroOrMore {
                capacity: cap(self.default_capacity),
            },
            Some(AstOcc::OneOrMore) => Occurrence::OneOrMore {
                capacity: cap(self.default_capacity),
            },
            Some(AstOcc::Bounded { min, max }) => {
                let mn = min.unwrap_or(0) as u32;
                let mx = max.map(|n| n as u32);
                // If min == max we can use a fixed capacity
                let capacity = if let (Some(mn2), Some(mx2)) = (min, max) {
                    if mn2 == mx2 {
                        Capacity::Fixed(*mx2 as usize)
                    } else {
                        cap(mx.unwrap_or(self.default_capacity as u32) as usize)
                    }
                } else {
                    cap(self.default_capacity)
                };
                Occurrence::Bounded { min: mn, max: mx, capacity }
            }
        }
    }
}

// ── Topological sort ──────────────────────────────────────────────────────────

fn topo_sort(types: &IndexMap<SmolStr, TypeDef>) -> Result<Vec<SmolStr>, IrError> {
    let mut order:   Vec<SmolStr>              = Vec::new();
    let mut visited: HashSet<SmolStr>          = HashSet::new();
    let mut in_stack: HashSet<SmolStr>         = HashSet::new();

    for name in types.keys() {
        if !visited.contains(name.as_str()) {
            topo_visit(name, types, &mut order, &mut visited, &mut in_stack)?;
        }
    }
    Ok(order)
}

fn topo_visit(
    name:     &SmolStr,
    types:    &IndexMap<SmolStr, TypeDef>,
    order:    &mut Vec<SmolStr>,
    visited:  &mut HashSet<SmolStr>,
    in_stack: &mut HashSet<SmolStr>,
) -> Result<(), IrError> {
    if in_stack.contains(name.as_str()) {
        // Cycle detected — not a hard error for CBOR (mutual refs are ok in
        // practice for some targets), just emit in current order.
        return Ok(());
    }
    if visited.contains(name.as_str()) {
        return Ok(());
    }

    in_stack.insert(name.clone());

    if let Some(def) = types.get(name.as_str()) {
        for dep in collect_deps(def) {
            if types.contains_key(dep.as_str()) && !visited.contains(dep.as_str()) {
                topo_visit(&dep, types, order, visited, in_stack)?;
            }
        }
    }

    in_stack.remove(name.as_str());
    visited.insert(name.clone());
    order.push(name.clone());
    Ok(())
}

fn collect_deps(def: &TypeDef) -> Vec<SmolStr> {
    let mut deps = Vec::new();
    match def {
        TypeDef::Struct(s) => {
            for f in &s.fields {
                collect_typeref_deps(&f.ty, &mut deps);
            }
        }
        TypeDef::Enum(e) => {
            for v in &e.variants {
                collect_typeref_deps(&v.ty, &mut deps);
            }
        }
        TypeDef::Array(a)  => collect_typeref_deps(&a.element, &mut deps),
        TypeDef::Map(m)    => {
            collect_typeref_deps(&m.key_ty, &mut deps);
            collect_typeref_deps(&m.val_ty, &mut deps);
        }
        TypeDef::Alias(a)  => collect_typeref_deps(&a.ty, &mut deps),
    }
    deps
}

fn collect_typeref_deps(tr: &TypeRef, out: &mut Vec<SmolStr>) {
    match tr {
        TypeRef::Named(n)         => out.push(n.clone()),
        TypeRef::Choice(choices)  => choices.iter().for_each(|c| collect_typeref_deps(c, out)),
        TypeRef::Tagged(_, inner) => collect_typeref_deps(inner, out),
        TypeRef::Primitive(_)     => {}
    }
}

// ── Pragma metadata extraction ────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
struct PragmaMeta {
    capacity:     Option<usize>,
    str_capacity: Option<usize>,
    regex_hook:   Option<String>,
    doc:          Option<String>,
    deprecated:   bool,
    tag:          Option<CborTag>,
}

impl PragmaMeta {
    fn from_pragmas(pragmas: &[Pragma]) -> Self {
        let mut m = Self::default();
        for p in pragmas {
            match &p.value {
                PragmaValue::Capacity(n)    => m.capacity     = Some(*n as usize),
                PragmaValue::StrCapacity(n) => m.str_capacity = Some(*n as usize),
                PragmaValue::RegexHook(s)   => m.regex_hook   = Some(s.clone()),
                PragmaValue::Doc(s)         => m.doc          = Some(s.clone()),
                PragmaValue::Deprecated     => m.deprecated   = true,
                _ => {}
            }
        }
        m
    }

    fn from_pragmas_with_capacity(
        pragmas:      &[Pragma],
        cap:          Option<usize>,
        str_cap:      Option<usize>,
    ) -> Self {
        let mut m = Self::from_pragmas(pragmas);
        if m.capacity.is_none()     { m.capacity     = cap; }
        if m.str_capacity.is_none() { m.str_capacity = str_cap; }
        m
    }
}

// ── Utility helpers ───────────────────────────────────────────────────────────

fn pascal_case(s: &str) -> String {
    s.split(|c: char| c == '-' || c == '_' || c == ' ')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                None    => String::new(),
                Some(f) => f.to_uppercase().collect::<String>() + chars.as_str(),
            }
        })
        .collect()
}

fn literal_to_typeref(lit: &LiteralValue) -> TypeRef {
    match lit {
        LiteralValue::Bool(_)  => TypeRef::Primitive(Primitive::Bool),
        LiteralValue::Null     => TypeRef::Primitive(Primitive::Null),
        LiteralValue::Uint(_)  => TypeRef::Primitive(Primitive::Uint),
        LiteralValue::Int(_)   => TypeRef::Primitive(Primitive::Int),
        LiteralValue::Float(_) => TypeRef::Primitive(Primitive::Float),
        LiteralValue::Bytes(_) => TypeRef::Primitive(Primitive::Bstr),
        LiteralValue::Text(_)  => TypeRef::Primitive(Primitive::Tstr),
    }
}

fn primitive_to_prelude_name(p: &Primitive) -> &'static str {
    match p {
        Primitive::Bool      => "bool",
        Primitive::Null      => "null",
        Primitive::Undefined => "undefined",
        Primitive::Uint      => "uint",
        Primitive::Int       => "int",
        Primitive::Float16   => "float16",
        Primitive::Float32   => "float32",
        Primitive::Float64   => "float64",
        Primitive::Float     => "float",
        Primitive::Bstr      => "bstr",
        Primitive::Tstr      => "tstr",
        Primitive::Any       => "any",
    }
}

fn type_to_name(ty: &Type) -> String {
    if ty.choices.len() == 1 {
        match &ty.choices[0].node.base.node {
            Type2::Typename { name, .. } => name.node.as_str().replace('-', "_"),
            Type2::Value(Value::Uint(n)) => format!("u{n}"),
            Type2::Value(Value::Int(n))  => format!("i{n}"),
            _                            => "anon".into(),
        }
    } else {
        "choice".into()
    }
}
