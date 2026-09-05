use syn::{Attribute, LitStr, Result};

pub(crate) fn parse_phase_attr_optional(attrs: &[Attribute]) -> Result<PhaseAttrOptional> {
    let mut name = None;
    let mut description = None;
    let mut terminal = false;
    let mut default = false;

    for attr in attrs {
        if !attr.path().is_ident("phase") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("name") {
                name = Some(meta.value()?.parse()?);
                return Ok(());
            }
            if meta.path.is_ident("description") {
                description = Some(meta.value()?.parse()?);
                return Ok(());
            }
            if meta.path.is_ident("terminal") {
                terminal = true;
                return Ok(());
            }
            if meta.path.is_ident("default") {
                default = true;
                return Ok(());
            }
            Err(meta.error("unsupported phase attribute"))
        })?;
    }

    Ok(PhaseAttrOptional {
        name,
        description,
        terminal,
        default,
    })
}

pub(crate) struct PhaseAttrOptional {
    pub(crate) name: Option<LitStr>,
    pub(crate) description: Option<LitStr>,
    pub(crate) terminal: bool,
    pub(crate) default: bool,
}

pub(crate) fn to_kebab_case(name: &str) -> String {
    let mut output = String::new();
    for (index, ch) in name.chars().enumerate() {
        if ch.is_uppercase() {
            if index != 0 {
                output.push('-');
            }
            for lower in ch.to_lowercase() {
                output.push(lower);
            }
        } else {
            output.push(ch);
        }
    }
    output
}

pub(crate) fn to_pascal_case(name: &str) -> String {
    let mut output = String::new();
    let mut uppercase_next = true;
    for ch in name.chars() {
        if ch == '_' || ch == '-' {
            uppercase_next = true;
            continue;
        }
        if uppercase_next {
            output.extend(ch.to_uppercase());
            uppercase_next = false;
        } else {
            output.push(ch);
        }
    }
    output
}
