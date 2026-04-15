#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldType {
    U8,
    U32Le,
    U64Le,
    I64Le,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FieldDesc {
    pub name: &'static str,
    pub offset: usize,
    pub ty: FieldType,
}

impl FieldDesc {
    pub const fn end_offset(self) -> usize {
        self.offset + self.ty.width()
    }
}

impl FieldType {
    pub const fn width(self) -> usize {
        match self {
            Self::U8 => 1,
            Self::U32Le => 4,
            Self::U64Le | Self::I64Le => 8,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventTemplate {
    Add,
    Mod,
    Del,
    Trade,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageDesc {
    pub template_id: u16,
    pub name: &'static str,
    pub min_block_len: usize,
    pub event_template: EventTemplate,
    pub fields: &'static [FieldDesc],
}

impl MessageDesc {
    pub fn validate(self) -> Result<(), &'static str> {
        for field in self.fields {
            if field.end_offset() > self.min_block_len {
                return Err(field.name);
            }
        }
        Ok(())
    }
}

pub const EOBI_SCHEMA_ID: u16 = 1;
pub const EOBI_SCHEMA_VERSION: u16 = 1;

pub const EOBI_ADD_FIELDS: &[FieldDesc] = &[
    FieldDesc {
        name: "order_id",
        offset: 0,
        ty: FieldType::U64Le,
    },
    FieldDesc {
        name: "instr",
        offset: 8,
        ty: FieldType::U32Le,
    },
    FieldDesc {
        name: "side",
        offset: 12,
        ty: FieldType::U8,
    },
    FieldDesc {
        name: "px",
        offset: 13,
        ty: FieldType::I64Le,
    },
    FieldDesc {
        name: "qty",
        offset: 21,
        ty: FieldType::I64Le,
    },
];

pub const EOBI_MOD_FIELDS: &[FieldDesc] = &[
    FieldDesc {
        name: "order_id",
        offset: 0,
        ty: FieldType::U64Le,
    },
    FieldDesc {
        name: "qty",
        offset: 8,
        ty: FieldType::I64Le,
    },
];

pub const EOBI_DEL_FIELDS: &[FieldDesc] = &[FieldDesc {
    name: "order_id",
    offset: 0,
    ty: FieldType::U64Le,
}];

pub const EOBI_TRADE_FIELDS: &[FieldDesc] = &[
    FieldDesc {
        name: "instr",
        offset: 0,
        ty: FieldType::U32Le,
    },
    FieldDesc {
        name: "px",
        offset: 4,
        ty: FieldType::I64Le,
    },
    FieldDesc {
        name: "qty",
        offset: 12,
        ty: FieldType::I64Le,
    },
    FieldDesc {
        name: "maker_order_id",
        offset: 20,
        ty: FieldType::U64Le,
    },
    FieldDesc {
        name: "taker_side",
        offset: 28,
        ty: FieldType::U8,
    },
];

pub const EOBI_MESSAGES: &[MessageDesc] = &[
    MessageDesc {
        template_id: 1001,
        name: "add_order",
        min_block_len: 29,
        event_template: EventTemplate::Add,
        fields: EOBI_ADD_FIELDS,
    },
    MessageDesc {
        template_id: 1002,
        name: "modify_order",
        min_block_len: 16,
        event_template: EventTemplate::Mod,
        fields: EOBI_MOD_FIELDS,
    },
    MessageDesc {
        template_id: 1003,
        name: "delete_order",
        min_block_len: 8,
        event_template: EventTemplate::Del,
        fields: EOBI_DEL_FIELDS,
    },
    MessageDesc {
        template_id: 1004,
        name: "trade",
        min_block_len: 29,
        event_template: EventTemplate::Trade,
        fields: EOBI_TRADE_FIELDS,
    },
];

#[inline]
pub fn eobi_message(template_id: u16) -> Option<&'static MessageDesc> {
    EOBI_MESSAGES
        .iter()
        .find(|desc| desc.template_id == template_id)
}

pub fn validate_eobi_schema() -> Result<(), String> {
    for desc in EOBI_MESSAGES {
        desc.validate()
            .map_err(|field| format!("{} field {} exceeds min block len", desc.name, field))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eobi_descriptors_are_in_bounds() {
        validate_eobi_schema().unwrap();
    }

    #[test]
    fn eobi_lookup_finds_supported_templates() {
        assert_eq!(
            eobi_message(1001).unwrap().event_template,
            EventTemplate::Add
        );
        assert_eq!(
            eobi_message(1002).unwrap().event_template,
            EventTemplate::Mod
        );
        assert_eq!(
            eobi_message(1003).unwrap().event_template,
            EventTemplate::Del
        );
        assert_eq!(
            eobi_message(1004).unwrap().event_template,
            EventTemplate::Trade
        );
        assert!(eobi_message(9999).is_none());
    }
}
