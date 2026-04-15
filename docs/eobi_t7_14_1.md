# Deutsche Boerse T7 14.1 EOBI Decoder

The EOBI decoder is generated from the official T7 Release 14.1 Enhanced Order
Book Interface XML representation package published by Eurex on 2026-02-27:

<https://www.eurex.com/resource/blob/4978116/6671416fc57f5ae1c52c76f16b9acb88/data/T7_R.14.1_%20EOBI_XML_Representation_Version_1.zip>

Generation command:

```bash
unzip -p T7_R.14.1_%20EOBI_XML_Representation_Version_1.zip eobi/eobi.xml \
  > /tmp/eobi.xml
python3 tools/generate_eobi_schema.py /tmp/eobi.xml src/decoder_schema.rs
cargo fmt
```

The generated schema records interface version `14.1` and build number
`141.420.0.ga-141004040-68`.

The decoder consumes venue `MessageHeaderComp` framing:

```text
BodyLen    u16 little-endian, includes the message header
TemplateID u16 little-endian
MsgSeqNum  u32 little-endian
```

The default `config.toml` sequence extractor points at
`PacketHeader.ApplSeqNum` using offset `8`, length `4`, little-endian.

`SnapshotOrder` does not carry `SecurityID`; it is decoded using the latest
`InstrumentSummary` context in the same snapshot cycle.

The engine still normalizes venue order identity into a `u64` order key. For
EOBI this key is a deterministic hash of `SecurityID`, `Side`, and
`TrdRegTSTimePriority`, matching the identity tuple defined by the venue while
preserving the current internal OBO event model.
