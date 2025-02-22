// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0
use anyhow::{anyhow, Result};
use clap::*;

use prometheus::Registry;

use std::sync::Arc;

use sui_benchmark::drivers::bench_driver::BenchDriver;
use sui_benchmark::drivers::driver::Driver;
use sui_benchmark::drivers::BenchmarkCmp;
use sui_benchmark::drivers::BenchmarkStats;

use sui_node::metrics;

use sui_benchmark::benchmark_setup::Env;
use sui_benchmark::options::Opts;

use sui_benchmark::workloads::workload_configuration::WorkloadConfiguration;

use tokio::runtime::Builder;
use tokio::sync::Barrier;

/// To spin up a local cluster and direct some load
/// at it with 50/50 shared and owned traffic, use
/// it something like:
/// ```cargo run  --release  --package sui-benchmark
/// --bin stress -- --num-client-threads 12 \
/// --num-server-threads 10 \
/// --num-transfer-accounts 2 \
/// bench \
/// --target-qps 100 \
/// --in-flight-ratio 2 \
/// --shared-counter 50 \
/// --transfer-object 50```
/// To point the traffic to an already running cluster,
/// use it something like:
/// ```cargo run  --release  --package sui-benchmark --bin stress -- --num-client-threads 12 \
/// --num-server-threads 10 \
/// --num-transfer-accounts 2 \
/// --primary-gas-id 0x59931dcac57ba20d75321acaf55e8eb5a2c47e9f \
/// --genesis-blob-path /tmp/genesis.blob \
/// --keystore-path /tmp/sui.keystore bench \
/// --target-qps 100 \
/// --in-flight-ratio 2 \
/// --shared-counter 50 \
/// --transfer-object 50```
#[tokio::main]
async fn main() -> Result<()> {
    let opts: Opts = Opts::parse();
    let mut config = telemetry_subscribers::TelemetryConfig::new();
    config.log_string = Some("warn".to_string());
    if !opts.log_path.is_empty() {
        config.log_file = Some(opts.log_path.clone());
    }
    let _guard = config.with_env().init();

    let registry_service = metrics::start_prometheus_server(
        format!("{}:{}", opts.client_metric_host, opts.client_metric_port)
            .parse()
            .unwrap(),
    );
    let registry: Registry = registry_service.default_registry();

    let barrier = Arc::new(Barrier::new(2));
    let cloned_barrier = barrier.clone();
    let env = if opts.local { Env::Local } else { Env::Remote };
    let benchmark_setup = env.setup(cloned_barrier, &registry, &opts).await?;
    let stress_stat_collection = opts.stress_stat_collection;
    barrier.wait().await;
    // create client runtime
    let client_runtime = Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(32 * 1024 * 1024)
        .worker_threads(opts.num_client_threads as usize)
        .build()
        .unwrap();
    let prev_benchmark_stats_path = opts.compare_with.clone();
    let curr_benchmark_stats_path = opts.benchmark_stats_path.clone();
    let registry_clone = registry.clone();
    let handle = std::thread::spawn(move || {
        client_runtime.block_on(async move {
            let workload_configuration = if opts.disjoint_mode {
                WorkloadConfiguration::Disjoint
            } else {
                WorkloadConfiguration::Combined
            };
            let workloads = workload_configuration
                .configure(
                    benchmark_setup.primary_gas,
                    benchmark_setup.pay_coin,
                    benchmark_setup.pay_coin_type_tag,
                    benchmark_setup.validator_proxy.clone(),
                    &opts,
                )
                .await?;
            let interval = opts.run_duration;
            // We only show continuous progress in stderr
            // if benchmark is running in unbounded mode,
            // otherwise summarized benchmark results are
            // published in the end
            let show_progress = interval.is_unbounded();
            let driver = BenchDriver::new(opts.stat_collection_interval, stress_stat_collection);
            driver
                .run(
                    workloads,
                    benchmark_setup.validator_proxy.clone(),
                    &registry_clone,
                    show_progress,
                    interval,
                )
                .await
        })
    });
    let joined = handle.join();
    if let Err(err) = joined {
        Err(anyhow!("Failed to join client runtime: {:?}", err))
    } else {
        let (benchmark_stats, stress_stats) = joined.unwrap().unwrap();
        let benchmark_table = benchmark_stats.to_table();
        eprintln!("Benchmark Report:");
        eprintln!("{}", benchmark_table);

        if stress_stat_collection {
            eprintln!("Stress Performance Report:");
            let stress_stats_table = stress_stats.to_table();
            eprintln!("{}", stress_stats_table);
        }

        if !prev_benchmark_stats_path.is_empty() {
            let data = std::fs::read_to_string(&prev_benchmark_stats_path)?;
            let prev_stats: BenchmarkStats = serde_json::from_str(&data)?;
            let cmp = BenchmarkCmp {
                new: &benchmark_stats,
                old: &prev_stats,
            };
            let cmp_table = cmp.to_table();
            eprintln!(
                "Benchmark Comparison Report[{}]:",
                prev_benchmark_stats_path
            );
            eprintln!("{}", cmp_table);
        }
        if !curr_benchmark_stats_path.is_empty() {
            let serialized = serde_json::to_string(&benchmark_stats)?;
            std::fs::write(curr_benchmark_stats_path, serialized)?;
        }
        Ok(())
    }
}
