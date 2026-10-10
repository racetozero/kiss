//! Release-mode performance coverage for the shared SDK dispatcher and RPC path.

#![cfg(all(feature = "mock", feature = "rpc"))]

use kiss_sdk::mock::{MockProvider, MockScript};
use kiss_sdk::{Client, Command, Session, SessionOptions};
use std::hint::black_box;
use std::sync::Arc;
use std::time::Instant;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

const SAMPLES: usize = 15;
const SDK_ITERATIONS: usize = 10_000;
const RPC_ITERATIONS: usize = 1_000;

async fn benchmark_session() -> (tempfile::TempDir, MockProvider, Arc<Session>) {
    let directory = tempfile::tempdir().expect("temporary benchmark directory");
    let provider = MockProvider::start(directory.path(), MockScript::text("ok"))
        .await
        .expect("mock provider starts");
    let session = Session::create(SessionOptions {
        cwd: directory.path().to_path_buf(),
        model: Some("mock/mock-1".into()),
        models_file: Some(provider.catalog_path()),
        no_context_files: true,
        trust_project_files: false,
        ..Default::default()
    })
    .await
    .expect("benchmark session builds");
    (directory, provider, session)
}

async fn measure_sdk(session: &Arc<Session>) {
    for _ in 0..100 {
        black_box(session.execute(Command::Ping {}).await);
    }

    let mut elapsed = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let started = Instant::now();
        for _ in 0..SDK_ITERATIONS {
            black_box(session.execute(Command::Ping {}).await);
        }
        elapsed.push(started.elapsed().as_nanos() / SDK_ITERATIONS as u128);
    }
    kiss_bench::report(
        "sdk_dispatch_ping",
        &mut elapsed,
        SDK_ITERATIONS,
        "shared_in_process_dispatcher",
    );
}

async fn rpc_round_trip(session: Arc<Session>) -> Vec<u128> {
    let (mut input, server_input) = tokio::io::duplex(64 * 1024);
    let (server_output, output) = tokio::io::duplex(64 * 1024);
    let server = tokio::spawn(async move {
        kiss_sdk::rpc::serve_streams(session, server_input, server_output)
            .await
            .expect("RPC benchmark server runs");
    });
    let mut output = BufReader::new(output);
    let mut client = Client::new();

    let mut elapsed = Vec::with_capacity(SAMPLES);
    for sample in 0..=SAMPLES {
        let iterations = if sample == 0 { 100 } else { RPC_ITERATIONS };
        let started = Instant::now();
        for _ in 0..iterations {
            let (_, line) = client.encode(Command::Ping {});
            input
                .write_all(line.as_bytes())
                .await
                .expect("RPC request writes");
            input.write_all(b"\n").await.expect("RPC delimiter writes");

            let mut response = String::new();
            output
                .read_line(&mut response)
                .await
                .expect("RPC response reads");
            black_box(
                client
                    .decode(response.trim_end())
                    .expect("response decodes"),
            );
        }
        if sample > 0 {
            elapsed.push(started.elapsed().as_nanos() / iterations as u128);
        }
    }

    drop(input);
    server.await.expect("RPC benchmark task joins");
    elapsed
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "release-mode performance benchmark"]
async fn benchmark_performance_sdk_and_rpc_dispatch() {
    let (_directory, _provider, session) = benchmark_session().await;
    measure_sdk(&session).await;
    let mut rpc_samples = rpc_round_trip(session).await;
    kiss_bench::report(
        "rpc_jsonl_ping_round_trip",
        &mut rpc_samples,
        RPC_ITERATIONS,
        "client_codec_in_memory_duplex_server",
    );
}
