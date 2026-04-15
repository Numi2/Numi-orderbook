use anyhow::Context;
use orderbook::insights::{
    validate_liquidity_pull_replay, LiquidityPullConfig, LiquidityPullReplayValidation,
};
use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::PathBuf;

const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

#[derive(Debug)]
struct Args {
    frame_path: PathBuf,
    liquidity_pull: LiquidityPullConfig,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse()?;
    let frames = read_recorded_frames(&args.frame_path)?;
    let validation = validate_liquidity_pull_replay(&frames, args.liquidity_pull);
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
            liquidity_pull: LiquidityPullConfig::default(),
        };

        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "-h" | "--help" => {
                    usage();
                    std::process::exit(0);
                }
                "--window-ms" => {
                    parsed.liquidity_pull.window_ns =
                        parse_next::<u64>(&mut args, "--window-ms")?.saturating_mul(1_000_000)
                }
                "--min-pulled-qty" => {
                    parsed.liquidity_pull.min_pulled_qty =
                        parse_next(&mut args, "--min-pulled-qty")?
                }
                "--min-pull-events" => {
                    parsed.liquidity_pull.min_pull_events =
                        parse_next(&mut args, "--min-pull-events")?
                }
                "--min-visible-qty" => {
                    parsed.liquidity_pull.min_visible_qty =
                        parse_next(&mut args, "--min-visible-qty")?
                }
                "--min-pull-ratio-bps" => {
                    parsed.liquidity_pull.min_pull_ratio_bps =
                        parse_next(&mut args, "--min-pull-ratio-bps")?
                }
                "--max-execution-ratio-bps" => {
                    parsed.liquidity_pull.max_execution_ratio_bps =
                        parse_next(&mut args, "--max-execution-ratio-bps")?
                }
                "--max-visible-after-ratio-bps" => {
                    parsed.liquidity_pull.max_visible_after_ratio_bps =
                        parse_next(&mut args, "--max-visible-after-ratio-bps")?
                }
                "--cooldown-ms" => {
                    parsed.liquidity_pull.cooldown_ns =
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
        "usage: liquidity_pull_replay recorded_frames.bin [threshold options]\n\
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

fn validation_passed(validation: &LiquidityPullReplayValidation) -> bool {
    validation.deterministic && validation.first.parse_errors == 0
}
