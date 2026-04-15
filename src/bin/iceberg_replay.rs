use anyhow::Context;
use orderbook::insights::{validate_iceberg_replay, IcebergConfig, IcebergReplayValidation};
use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::PathBuf;

const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

#[derive(Debug)]
struct Args {
    frame_path: PathBuf,
    iceberg: IcebergConfig,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse()?;
    let frames = read_recorded_frames(&args.frame_path)?;
    let validation = validate_iceberg_replay(&frames, args.iceberg);
    println!("{}", serde_json::to_string_pretty(&validation)?);
    if !validation_passed(&validation) {
        std::process::exit(2);
    }
    Ok(())
}

impl Args {
    fn parse() -> anyhow::Result<Self> {
        let mut parsed = Self {
            frame_path: PathBuf::new(),
            iceberg: IcebergConfig::default(),
        };

        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "-h" | "--help" => {
                    usage();
                    std::process::exit(0);
                }
                "--window-ms" => {
                    parsed.iceberg.window_ns =
                        parse_next::<u64>(&mut args, "--window-ms")?.saturating_mul(1_000_000)
                }
                "--min-executed-qty" => {
                    parsed.iceberg.min_executed_qty = parse_next(&mut args, "--min-executed-qty")?
                }
                "--min-execute-events" => {
                    parsed.iceberg.min_execute_events =
                        parse_next(&mut args, "--min-execute-events")?
                }
                "--min-replenish-events" => {
                    parsed.iceberg.min_replenish_events =
                        parse_next(&mut args, "--min-replenish-events")?
                }
                "--min-replenished-qty" => {
                    parsed.iceberg.min_replenished_qty =
                        parse_next(&mut args, "--min-replenished-qty")?
                }
                "--min-replenishment-ratio-bps" => {
                    parsed.iceberg.min_replenishment_ratio_bps =
                        parse_next(&mut args, "--min-replenishment-ratio-bps")?
                }
                "--min-over-display-ratio-bps" => {
                    parsed.iceberg.min_over_display_ratio_bps =
                        parse_next(&mut args, "--min-over-display-ratio-bps")?
                }
                "--max-pull-ratio-bps" => {
                    parsed.iceberg.max_pull_ratio_bps =
                        parse_next(&mut args, "--max-pull-ratio-bps")?
                }
                "--cooldown-ms" => {
                    parsed.iceberg.cooldown_ns =
                        parse_next::<u64>(&mut args, "--cooldown-ms")?.saturating_mul(1_000_000)
                }
                other if parsed.frame_path.as_os_str().is_empty() => {
                    parsed.frame_path = PathBuf::from(other);
                }
                other => anyhow::bail!("unknown argument {other:?}"),
            }
        }

        if parsed.frame_path.as_os_str().is_empty() {
            usage();
            anyhow::bail!("missing frame recording path");
        }
        Ok(parsed)
    }
}

fn usage() {
    eprintln!(
        "usage: iceberg_replay recorded_frames.bin [threshold options]\n\
The input format is repeated little-endian u32 frame length followed by one raw-v1 frame.\n\
Use absorption_sidecar --record-frames PATH to create a compatible recording."
    );
}

fn parse_next<T>(args: &mut impl Iterator<Item = String>, flag: &str) -> anyhow::Result<T>
where
    T: std::str::FromStr,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    args.next()
        .ok_or_else(|| anyhow::anyhow!("{flag} requires a value"))?
        .parse::<T>()
        .with_context(|| format!("parse {flag} value"))
}

fn read_recorded_frames(path: &PathBuf) -> anyhow::Result<Vec<Vec<u8>>> {
    let file = File::open(path).with_context(|| format!("open frame recording {path:?}"))?;
    let mut reader = BufReader::new(file);
    let mut frames = Vec::new();
    loop {
        let Some(len) = read_len(&mut reader)? else {
            break;
        };
        let len = usize::try_from(len).context("recorded frame length does not fit usize")?;
        if len > MAX_FRAME_LEN {
            anyhow::bail!("recorded frame length {len} exceeds max {MAX_FRAME_LEN}");
        }
        let mut frame = vec![0_u8; len];
        reader
            .read_exact(&mut frame)
            .with_context(|| format!("read recorded frame payload len={len}"))?;
        frames.push(frame);
    }
    Ok(frames)
}

fn read_len(reader: &mut impl Read) -> anyhow::Result<Option<u32>> {
    let mut bytes = [0_u8; 4];
    let mut read = 0;
    while read < bytes.len() {
        match reader.read(&mut bytes[read..]) {
            Ok(0) if read == 0 => return Ok(None),
            Ok(0) => {
                return Err(
                    io::Error::new(io::ErrorKind::UnexpectedEof, "partial frame length").into(),
                );
            }
            Ok(n) => read += n,
            Err(err) => return Err(err).context("read recorded frame length"),
        }
    }
    Ok(Some(u32::from_le_bytes(bytes)))
}

fn validation_passed(validation: &IcebergReplayValidation) -> bool {
    validation.deterministic && validation.first.parse_errors == 0
}
