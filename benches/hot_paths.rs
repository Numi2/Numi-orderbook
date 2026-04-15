use criterion::{black_box, criterion_group, criterion_main, BatchSize, Criterion};
use orderbook::bench_support::{benchmark_order_book, BenchmarkFixtures, FixtureConfig};
use orderbook::codec_raw::{channel_id, msg_type};
use orderbook::decoder_eobi::EobiSbeDecoder;
use orderbook::decoder_fast::FastEmdiDecoder;
use orderbook::decoder_itch::Itch50Decoder;
use orderbook::parser::MessageDecoder;
use orderbook::pool::PacketPool;
use orderbook::pubsub::Bus;
use orderbook::spsc::SpscQueue;
use std::time::Duration;

fn criterion_config() -> Criterion {
    let smoke = std::env::var_os("NUMI_BENCH_SMOKE").is_some();
    let sample_size = if smoke { 10 } else { 30 };
    let measurement = if smoke {
        Duration::from_millis(400)
    } else {
        Duration::from_secs(2)
    };
    Criterion::default()
        .sample_size(sample_size)
        .warm_up_time(Duration::from_millis(100))
        .measurement_time(measurement)
}

fn bench_order_book_mixed_l3(c: &mut Criterion) {
    let cfg = FixtureConfig {
        instruments: 64,
        orders_per_instrument: 128,
        packet_count: 2048,
        messages_per_packet: 4,
        ..FixtureConfig::default()
    };
    let fixtures = BenchmarkFixtures::new(cfg);
    c.bench_function("orderbook/mixed_l3_apply", |b| {
        b.iter_batched(
            || benchmark_order_book(cfg),
            |mut book| {
                for event in &fixtures.events {
                    book.apply(black_box(event));
                }
                black_box(book.state_hash())
            },
            BatchSize::SmallInput,
        );
    });
}

fn bench_decoders(c: &mut Criterion) {
    let cfg = FixtureConfig {
        instruments: 32,
        orders_per_instrument: 64,
        packet_count: 512,
        messages_per_packet: 4,
        ..FixtureConfig::default()
    };
    let fixtures = BenchmarkFixtures::new(cfg);

    c.bench_function("decoder/eobi_primary_mix", |b| {
        b.iter_batched(
            || (EobiSbeDecoder::new(), Vec::with_capacity(64)),
            |(decoder, mut out)| {
                for payload in &fixtures.eobi_packets {
                    decoder.decode_messages(black_box(payload), &mut out);
                    black_box(out.len());
                    out.clear();
                }
            },
            BatchSize::SmallInput,
        );
    });

    c.bench_function("decoder/itch50_representative_mix", |b| {
        b.iter_batched(
            || (Itch50Decoder::new(), Vec::with_capacity(2048)),
            |(decoder, mut out)| {
                decoder.decode_messages(black_box(&fixtures.itch_payload), &mut out);
                black_box(out.len());
            },
            BatchSize::SmallInput,
        );
    });

    c.bench_function("decoder/fast_like_representative_mix", |b| {
        b.iter_batched(
            || (FastEmdiDecoder::new(), Vec::with_capacity(2048)),
            |(decoder, mut out)| {
                decoder.decode_messages(black_box(&fixtures.fast_payload), &mut out);
                black_box(out.len());
            },
            BatchSize::SmallInput,
        );
    });
}

fn bench_queue_and_pool(c: &mut Criterion) {
    c.bench_function("spsc/push_pop_batch_u64", |b| {
        b.iter_batched(
            || (SpscQueue::new(2048), Vec::<u64>::with_capacity(64)),
            |(queue, mut out)| {
                for value in 0..1024_u64 {
                    queue.push(black_box(value)).expect("queue has capacity");
                }
                while let Some(value) = queue.pop() {
                    out.push(value);
                }
                black_box(out.len());
            },
            BatchSize::SmallInput,
        );
    });

    c.bench_function("packet_pool/get_put_burst", |b| {
        b.iter_batched(
            || (PacketPool::new(4096, 2048).unwrap(), Vec::with_capacity(64)),
            |(pool, mut held)| {
                for _ in 0..64 {
                    held.push(pool.get());
                }
                while let Some(buf) = held.pop() {
                    pool.put(buf);
                }
                black_box(pool.available());
            },
            BatchSize::SmallInput,
        );
    });
}

fn bench_pubsub_raw_obo(c: &mut Criterion) {
    let fixtures = BenchmarkFixtures::new(FixtureConfig::default());
    c.bench_function("pubsub/raw_v1_obo_publish", |b| {
        b.iter_batched(
            || {
                let bus = Bus::new(8192);
                bus.publisher()
            },
            |publisher| {
                for idx in 0..4096_u64 {
                    let payload =
                        &fixtures.raw_obo_payloads[idx as usize % fixtures.raw_obo_payloads.len()];
                    publisher.publish_raw(
                        msg_type::OBO_ADD,
                        channel_id::OBO_L3,
                        1,
                        idx + 1,
                        black_box(payload),
                    );
                }
                black_box(publisher.next_global_sequence());
            },
            BatchSize::SmallInput,
        );
    });
}

criterion_group! {
    name = hot_paths;
    config = criterion_config();
    targets =
        bench_order_book_mixed_l3,
        bench_decoders,
        bench_queue_and_pool,
        bench_pubsub_raw_obo
}
criterion_main!(hot_paths);
