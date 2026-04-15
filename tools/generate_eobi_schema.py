#!/usr/bin/env python3
"""Generate Rust EOBI layout descriptors from Deutsche Boerse T7 XML.

Usage:
  tools/generate_eobi_schema.py /path/to/eobi.xml src/decoder_schema.rs
"""

from __future__ import annotations

import re
import sys
import xml.etree.ElementTree as ET
from dataclasses import dataclass
from pathlib import Path


KIND_BY_MESSAGE = {
    "Heartbeat": "Heartbeat",
    "PacketHeader": "PacketHeader",
    "ProductStateChange": "ProductStateChange",
    "InstrumentStateChange": "InstrumentStateChange",
    "InstrumentSummary": "InstrumentSummary",
    "ProductSummary": "ProductSummary",
    "OrderAdd": "OrderAdd",
    "OrderModify": "OrderModify",
    "OrderModifySamePrio": "OrderModifySamePrio",
    "OrderDelete": "OrderDelete",
    "OrderMassDelete": "OrderMassDelete",
    "PartialOrderExecution": "PartialOrderExecution",
    "FullOrderExecution": "FullOrderExecution",
    "SnapshotOrder": "SnapshotOrder",
}


@dataclass(frozen=True)
class DataType:
    root_type: str
    size: int
    min_value: str


@dataclass(frozen=True)
class Field:
    name: str
    offset: int
    ty: str

    @property
    def const_name(self) -> str:
        name = re.sub(r"([a-z0-9])([A-Z])", r"\1_\2", self.name)
        out = []
        for ch in name:
            if ch.isalnum():
                out.append(ch.upper())
            else:
                out.append("_")
        return re.sub(r"_+", "_", "".join(out)).strip("_")


@dataclass(frozen=True)
class Message:
    name: str
    template_id: int
    kind: str
    fields: tuple[Field, ...]

    @property
    def mod_name(self) -> str:
        chars = []
        for i, ch in enumerate(self.name):
            if ch.isupper() and i > 0:
                chars.append("_")
            chars.append(ch.lower())
        return "".join(chars)

    @property
    def min_block_len(self) -> int:
        widths = {
            "I8": 1,
            "U8": 1,
            "I16Le": 2,
            "U16Le": 2,
            "I32Le": 4,
            "U32Le": 4,
            "I64Le": 8,
            "U64Le": 8,
            "Decimal64Le": 8,
        }
        max_end = 0
        for field in self.fields:
            if field.ty.startswith("FixedBytes("):
                width = int(field.ty[len("FixedBytes(") : -1])
            else:
                width = widths[field.ty]
            max_end = max(max_end, field.offset + width)
        return max_end


def rust_string(value: str) -> str:
    return value.replace("\\", "\\\\").replace('"', '\\"')


def field_type_for(data_type: DataType) -> str:
    if data_type.root_type == "floatDecimal":
        if data_type.size != 8:
            raise ValueError(f"unsupported decimal width {data_type.size}")
        return "Decimal64Le"
    if data_type.root_type == "String":
        return f"FixedBytes({data_type.size})"
    if data_type.root_type == "int":
        signed = data_type.min_value.startswith("-")
        match = {
            (1, True): "I8",
            (1, False): "U8",
            (2, True): "I16Le",
            (2, False): "U16Le",
            (4, True): "I32Le",
            (4, False): "U32Le",
            (8, True): "I64Le",
            (8, False): "U64Le",
        }
        return match[(data_type.size, signed)]
    raise ValueError(f"unsupported root type {data_type.root_type!r}")


def collect_members(elem: ET.Element, data_types: dict[str, DataType]) -> list[Field]:
    fields: list[Field] = []
    for child in elem:
        if child.tag == "Member":
            name = child.attrib["name"]
            if child.attrib.get("hidden") == "true" or "offset" not in child.attrib:
                continue
            data_type = data_types.get(child.attrib["type"])
            if data_type is None:
                continue
            fields.append(
                Field(
                    name=name,
                    offset=int(child.attrib["offset"]),
                    ty=field_type_for(data_type),
                )
            )
        elif child.tag == "Group":
            if "counter" in child.attrib:
                continue
            fields.extend(collect_members(child, data_types))
    deduped: dict[str, Field] = {}
    for field in fields:
        deduped.setdefault(field.name, field)
    return sorted(deduped.values(), key=lambda f: (f.offset, f.name))


def parse(xml_path: Path) -> tuple[str, str, list[Message]]:
    root = ET.parse(xml_path).getroot()
    version = root.attrib["version"]
    build_number = root.attrib["buildNumber"]

    data_types: dict[str, DataType] = {}
    for elem in root.findall("./DataTypes/DataType"):
        if "size" not in elem.attrib:
            continue
        data_types[elem.attrib["name"]] = DataType(
            root_type=elem.attrib["rootType"],
            size=int(elem.attrib["size"]),
            min_value=elem.attrib.get("minValue", ""),
        )

    messages: list[Message] = []
    for elem in root.findall("./ApplicationMessages/ApplicationMessage"):
        name = elem.attrib["name"]
        template_id = int(elem.attrib["numericID"])
        kind = KIND_BY_MESSAGE.get(name, "Unsupported")
        messages.append(
            Message(
                name=name,
                template_id=template_id,
                kind=kind,
                fields=tuple(collect_members(elem, data_types)),
            )
        )
    messages.sort(key=lambda msg: msg.template_id)
    return version, build_number, messages


def emit(version: str, build_number: str, messages: list[Message], source: Path) -> str:
    out: list[str] = []
    out.append("// @generated by tools/generate_eobi_schema.py; do not edit by hand.\n")
    out.append("// Source: Deutsche Boerse T7 EOBI XML representation.\n")
    out.append(f"// Input: {source.name}\n\n")
    out.append("#[derive(Debug, Clone, Copy, PartialEq, Eq)]\n")
    out.append("pub enum FieldType {\n")
    for variant in [
        "I8",
        "U8",
        "I16Le",
        "U16Le",
        "I32Le",
        "U32Le",
        "I64Le",
        "U64Le",
        "Decimal64Le",
    ]:
        out.append(f"    {variant},\n")
    out.append("    FixedBytes(usize),\n")
    out.append("}\n\n")
    out.append("impl FieldType {\n")
    out.append("    pub const fn width(self) -> usize {\n")
    out.append("        match self {\n")
    out.append("            Self::I8 | Self::U8 => 1,\n")
    out.append("            Self::I16Le | Self::U16Le => 2,\n")
    out.append("            Self::I32Le | Self::U32Le => 4,\n")
    out.append("            Self::I64Le | Self::U64Le | Self::Decimal64Le => 8,\n")
    out.append("            Self::FixedBytes(width) => width,\n")
    out.append("        }\n")
    out.append("    }\n")
    out.append("}\n\n")
    out.append("#[derive(Debug, Clone, Copy, PartialEq, Eq)]\n")
    out.append("pub struct FieldDesc {\n")
    out.append("    pub name: &'static str,\n")
    out.append("    pub offset: usize,\n")
    out.append("    pub ty: FieldType,\n")
    out.append("}\n\n")
    out.append("impl FieldDesc {\n")
    out.append("    pub const fn end_offset(self) -> usize {\n")
    out.append("        self.offset + self.ty.width()\n")
    out.append("    }\n")
    out.append("}\n\n")
    out.append("#[derive(Debug, Clone, Copy, PartialEq, Eq)]\n")
    out.append("pub enum EventTemplate {\n")
    for kind in [
        "Unsupported",
        "Heartbeat",
        "PacketHeader",
        "ProductStateChange",
        "InstrumentStateChange",
        "InstrumentSummary",
        "ProductSummary",
        "OrderAdd",
        "OrderModify",
        "OrderModifySamePrio",
        "OrderDelete",
        "OrderMassDelete",
        "PartialOrderExecution",
        "FullOrderExecution",
        "SnapshotOrder",
    ]:
        out.append(f"    {kind},\n")
    out.append("}\n\n")
    out.append("#[derive(Debug, Clone, Copy, PartialEq, Eq)]\n")
    out.append("pub struct MessageDesc {\n")
    out.append("    pub template_id: u16,\n")
    out.append("    pub name: &'static str,\n")
    out.append("    pub min_block_len: usize,\n")
    out.append("    pub event_template: EventTemplate,\n")
    out.append("    pub fields: &'static [FieldDesc],\n")
    out.append("}\n\n")
    out.append("impl MessageDesc {\n")
    out.append("    pub fn validate(self) -> Result<(), &'static str> {\n")
    out.append("        for field in self.fields {\n")
    out.append("            if field.end_offset() > self.min_block_len {\n")
    out.append("                return Err(field.name);\n")
    out.append("            }\n")
    out.append("        }\n")
    out.append("        Ok(())\n")
    out.append("    }\n")
    out.append("}\n\n")
    out.append(f'pub const EOBI_INTERFACE_VERSION: &str = "{rust_string(version)}";\n')
    out.append(f'pub const EOBI_BUILD_NUMBER: &str = "{rust_string(build_number)}";\n')
    out.append("pub const EOBI_MESSAGE_HEADER_LEN: usize = 8;\n\n")

    for msg in messages:
        out.append(f"pub const {msg.name.upper()}_FIELDS: &[FieldDesc] = &[\n")
        for field in msg.fields:
            out.append("    FieldDesc {\n")
            out.append(f'        name: "{field.name}",\n')
            out.append(f"        offset: {field.offset},\n")
            if field.ty.startswith("FixedBytes("):
                out.append(f"        ty: FieldType::{field.ty},\n")
            else:
                out.append(f"        ty: FieldType::{field.ty},\n")
            out.append("    },\n")
        out.append("];\n\n")

        out.append(f"pub mod {msg.mod_name} {{\n")
        out.append(f"    pub const TEMPLATE_ID: u16 = {msg.template_id};\n")
        out.append(f"    pub const MIN_BLOCK_LEN: usize = {msg.min_block_len};\n")
        for field in msg.fields:
            out.append(f"    pub const {field.const_name}_OFFSET: usize = {field.offset};\n")
        out.append("}\n\n")

    out.append("pub const EOBI_MESSAGES: &[MessageDesc] = &[\n")
    for msg in messages:
        out.append("    MessageDesc {\n")
        out.append(f"        template_id: {msg.template_id},\n")
        out.append(f'        name: "{msg.name}",\n')
        out.append(f"        min_block_len: {msg.min_block_len},\n")
        out.append(f"        event_template: EventTemplate::{msg.kind},\n")
        out.append(f"        fields: {msg.name.upper()}_FIELDS,\n")
        out.append("    },\n")
    out.append("];\n\n")
    out.append("#[inline]\n")
    out.append("pub fn eobi_message(template_id: u16) -> Option<&'static MessageDesc> {\n")
    out.append("    match template_id {\n")
    for idx, msg in enumerate(messages):
        out.append(f"        {msg.template_id} => Some(&EOBI_MESSAGES[{idx}]),\n")
    out.append("        _ => None,\n")
    out.append("    }\n")
    out.append("}\n\n")
    out.append("pub fn validate_eobi_schema() -> Result<(), String> {\n")
    out.append("    for desc in EOBI_MESSAGES {\n")
    out.append("        desc.validate()\n")
    out.append(
        '            .map_err(|field| format!("{} field {} exceeds min block len", desc.name, field))?;\n'
    )
    out.append("    }\n")
    out.append("    Ok(())\n")
    out.append("}\n\n")
    out.append("#[cfg(test)]\n")
    out.append("mod tests {\n")
    out.append("    use super::*;\n\n")
    out.append("    #[test]\n")
    out.append("    fn eobi_descriptors_are_in_bounds() {\n")
    out.append("        validate_eobi_schema().unwrap();\n")
    out.append("    }\n\n")
    out.append("    #[test]\n")
    out.append("    fn eobi_lookup_finds_generated_templates() {\n")
    for name in [
        "PacketHeader",
        "OrderAdd",
        "OrderModify",
        "OrderModifySamePrio",
        "OrderDelete",
        "OrderMassDelete",
        "PartialOrderExecution",
        "FullOrderExecution",
        "ProductStateChange",
        "InstrumentStateChange",
        "InstrumentSummary",
        "SnapshotOrder",
    ]:
        msg = next(m for m in messages if m.name == name)
        out.append(
            f"        assert_eq!(eobi_message({msg.template_id}).unwrap().event_template, EventTemplate::{msg.kind});\n"
        )
    out.append("        assert!(eobi_message(9999).is_none());\n")
    out.append("    }\n")
    out.append("}\n")
    return "".join(out)


def main(argv: list[str]) -> int:
    if len(argv) != 3:
        print(__doc__.strip(), file=sys.stderr)
        return 2
    source = Path(argv[1])
    target = Path(argv[2])
    version, build_number, messages = parse(source)
    target.write_text(emit(version, build_number, messages, source), encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
