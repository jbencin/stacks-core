#![allow(clippy::unwrap_used, clippy::expect_used)]
use std::fs::File;
use std::process::{Command, Stdio};

use blockstack_lib::chainstate::stacks::{
    StacksBlockHeader, MINER_BLOCK_CONSENSUS_HASH, MINER_BLOCK_HEADER_HASH,
};
use blockstack_lib::clarity_vm::database::marf::MarfedKV;
use clarity::consts::CHAIN_ID_TESTNET;
use clarity::types::StacksEpochId;
use clarity::vm::analysis::{run_analysis, AnalysisDatabase};
use clarity::vm::ast::build_ast_with_diagnostics;
use clarity::vm::contexts::GlobalContext;
use clarity::vm::costs::LimitedCostTracker;
use clarity::vm::database::{ClarityDatabase, MemoryBackingStore};
use clarity::vm::types::{QualifiedContractIdentifier, StandardPrincipalData};
use clarity::vm::{
    eval_all, CallStack, ClarityVersion, ContractContext, ContractName, Environment, Value,
};
use cmd_lib::run_cmd;
use criterion::{criterion_group, criterion_main, Criterion};
use datastore::{BurnDatastore, StacksConstants};
use pprof::criterion::{Output, PProfProfiler};
use rand::{thread_rng, Rng};
use stacks_common::types::chainstate::StacksBlockId;

mod datastore;

/// Scale benchmark by adjusting number of loops
const SCALE: usize = 1;

/// ### Obtaining a database
///
/// Read costs increase with the size of the database.
/// For meaningful benchmark results, it's best to use the actual mainnet database
/// This can be downloaded from https://archive.hiro.so/
const CLARITY_MARF_PATH: &str = "../../../data/mainnet/chainstate/vm/clarity/";

/// ### Finding a block hash
///
/// This needs to taken from the above database.
/// To query the hash of the block we want to use to use, run the following:
///
/// ```sh
/// echo "select * from marf_data" | sqlite3 chainstate/vm/clarity/marf.sqlite
/// pick second to last block hash as `READ_TIP``
/// ```
pub const READ_TIP: &str = "4bd4ccea6502d816d37770e532325264f3691de93a2bd361f11f7bbec161cb12";

/// Clear all fs cache.
/// Must be run as root!!!
/// Can use `sudo -E cargo bench` to do this
///
/// Args:
///  - `use_run_cmd`: Use simpler way to run shell command. Probably slower then `std::process::cmd`
#[cfg(target_os = "linux")]
fn clear_cache(use_run_cmd: bool) -> Result<(), &'static str> {
    // Run `sync; echo 3 > /proc/sys/vm/drop_caches`
    if use_run_cmd {
        run_cmd!(sync; echo 3 > /proc/sys/vm/drop_caches).map_err(|_| "Failed to execute process")
    } else {
        Command::new("sync")
            .output()
            .map_err(|_| "Failed to execute process")?;

        let file = File::create("/proc/sys/vm/drop_caches").map_err(|_| "Failed to open file")?;

        Command::new("echo")
            .arg("3")
            .stdout(Stdio::from(file))
            .output()
            .map_err(|_| "Failed to execute process")?;

        Ok(())
    }
}

fn read_bench_sequential(c: &mut Criterion) {
    let miner_tip = StacksBlockHeader::make_index_block_hash(
        &MINER_BLOCK_CONSENSUS_HASH,
        &MINER_BLOCK_HEADER_HASH,
    );
    let mut marfed_kv = MarfedKV::open(CLARITY_MARF_PATH, Some(&miner_tip), None).unwrap();

    // Set up Clarity Backing Store
    // NOTE: this StacksBlockId comes from the `block_headers` in the chainstate DB (db/index.sqlite)
    let read_tip = StacksBlockId::from_hex(READ_TIP).unwrap();
    let new_tip = StacksBlockId::from([5; 32]);
    let mut writeable_marf_store = marfed_kv.begin(&read_tip, &new_tip);

    let contract_id = QualifiedContractIdentifier::new(
        StandardPrincipalData::transient(),
        ContractName::from("fold-bench"),
    );
    let constants = StacksConstants::default();
    let burn_datastore = BurnDatastore::new(constants);
    let mut clarity_store = MemoryBackingStore::new();
    let mut conn =
        ClarityDatabase::new(&mut writeable_marf_store, &burn_datastore, &burn_datastore);
    conn.begin();
    conn.set_clarity_epoch_version(StacksEpochId::latest());
    conn.commit();
    let mut cost_tracker = LimitedCostTracker::new_free();
    let mut contract_context = ContractContext::new(contract_id.clone(), ClarityVersion::latest());

    let contract_str = std::fs::read_to_string("benches/contracts/large-map.clar").unwrap();

    // Parse the contract
    let (mut ast, _, success) = build_ast_with_diagnostics(
        &contract_id,
        &contract_str,
        &mut cost_tracker,
        ClarityVersion::latest(),
        StacksEpochId::latest(),
    );

    if !success {
        panic!("Failed to parse contract");
    }

    // Create a new analysis database
    let mut analysis_db = AnalysisDatabase::new(&mut clarity_store);

    // Run the analysis passes
    let mut contract_analysis = run_analysis(
        &contract_id,
        &mut ast.expressions,
        &mut analysis_db,
        false,
        cost_tracker,
        StacksEpochId::latest(),
        ClarityVersion::latest(),
    )
    .expect("Failed to run analysis");

    let mut global_context = GlobalContext::new(
        false,
        CHAIN_ID_TESTNET,
        conn,
        contract_analysis.cost_track.take().unwrap(),
        StacksEpochId::latest(),
    );

    global_context.begin();

    {
        // Initialize the contract
        eval_all(
            &ast.expressions,
            &mut contract_context,
            &mut global_context,
            None,
        )
        .expect("Failed to interpret the contract");

        let insert_list = contract_context
            .lookup_function("insert-list")
            .expect("failed to lookup function");
        let get_one = contract_context
            .lookup_function("get-one")
            .expect("failed to lookup function");

        let mut call_stack = CallStack::new();
        let mut env = Environment::new(
            &mut global_context,
            &contract_context,
            &mut call_stack,
            Some(StandardPrincipalData::transient().into()),
            Some(StandardPrincipalData::transient().into()),
            None,
        );

        // Insert a bunch of values into the map.
        // 8192 * 8192 values, each of which is 16 bytes = 1GB
        for i in 0..256 {
            print!("{}...", i * 8192);
            let list =
                Value::cons_list_unsanitized((i * 8192..(i + 1) * 8192).map(Value::Int).collect())
                    .expect("failed to construct list argument");
            insert_list
                .execute_apply(&[list], &mut env)
                .expect("Function call failed");
        }

        env.global_context.commit().expect("Commit failed");
        env.global_context.begin();
        println!("Data committed to ClarityDB");

        clear_cache(true).expect("Failed to clear fs cache");
        println!("Cache cleared");

        c.bench_function("get_one:sequential", |b| {
            //clear_cache(true).expect("Failed to clear fs cache");
            //println!("Cache cleared");

            b.iter(|| {
                for i in 0..SCALE {
                    let _result = get_one
                        .execute_apply(&[Value::Int(i as i128)], &mut env)
                        .expect("Function call failed");
                }
            });
        });
    }

    global_context.commit().unwrap();
}

fn read_bench_random(c: &mut Criterion) {
    let miner_tip = StacksBlockHeader::make_index_block_hash(
        &MINER_BLOCK_CONSENSUS_HASH,
        &MINER_BLOCK_HEADER_HASH,
    );
    let mut marfed_kv = MarfedKV::open(CLARITY_MARF_PATH, Some(&miner_tip), None).unwrap();

    // Set up Clarity Backing Store
    // NOTE: this StacksBlockId comes from the `block_headers` in the chainstate DB (db/index.sqlite)
    let read_tip = StacksBlockId::from_hex(READ_TIP).unwrap();
    let new_tip = StacksBlockId::from([5; 32]);
    let mut writeable_marf_store = marfed_kv.begin(&read_tip, &new_tip);

    let contract_id = QualifiedContractIdentifier::new(
        StandardPrincipalData::transient(),
        ContractName::from("fold-bench"),
    );
    let constants = StacksConstants::default();
    let burn_datastore = BurnDatastore::new(constants);
    let mut clarity_store = MemoryBackingStore::new();
    let mut conn =
        ClarityDatabase::new(&mut writeable_marf_store, &burn_datastore, &burn_datastore);
    conn.begin();
    conn.set_clarity_epoch_version(StacksEpochId::latest());
    conn.commit();
    let mut cost_tracker = LimitedCostTracker::new_free();
    let mut contract_context = ContractContext::new(contract_id.clone(), ClarityVersion::latest());

    let contract_str = std::fs::read_to_string("benches/contracts/large-map.clar").unwrap();

    // Parse the contract
    let (mut ast, _, success) = build_ast_with_diagnostics(
        &contract_id,
        &contract_str,
        &mut cost_tracker,
        ClarityVersion::latest(),
        StacksEpochId::latest(),
    );

    if !success {
        panic!("Failed to parse contract");
    }

    // Create a new analysis database
    let mut analysis_db = AnalysisDatabase::new(&mut clarity_store);

    // Run the analysis passes
    let mut contract_analysis = run_analysis(
        &contract_id,
        &mut ast.expressions,
        &mut analysis_db,
        false,
        cost_tracker,
        StacksEpochId::latest(),
        ClarityVersion::latest(),
    )
    .expect("Failed to run analysis");

    let mut global_context = GlobalContext::new(
        false,
        CHAIN_ID_TESTNET,
        conn,
        contract_analysis.cost_track.take().unwrap(),
        StacksEpochId::latest(),
    );

    global_context.begin();

    {
        // Initialize the contract
        eval_all(
            &ast.expressions,
            &mut contract_context,
            &mut global_context,
            None,
        )
        .expect("Failed to interpret the contract");

        let insert_list = contract_context
            .lookup_function("insert-list")
            .expect("failed to lookup function");
        let get_one = contract_context
            .lookup_function("get-one")
            .expect("failed to lookup function");

        let mut call_stack = CallStack::new();
        let mut env = Environment::new(
            &mut global_context,
            &contract_context,
            &mut call_stack,
            Some(StandardPrincipalData::transient().into()),
            Some(StandardPrincipalData::transient().into()),
            None,
        );

        // Insert a bunch of values into the map.
        // 8192 * 8192 values, each of which is 16 bytes = 1GB
        for i in 0..256 {
            print!("{}...", i * 8192);
            let list =
                Value::cons_list_unsanitized((i * 8192..(i + 1) * 8192).map(Value::Int).collect())
                    .expect("failed to construct list argument");
            insert_list
                .execute_apply(&[list], &mut env)
                .expect("Function call failed");
        }

        env.global_context.commit().expect("Commit failed");
        env.global_context.begin();

        clear_cache(true).expect("Failed to clear fs cache");
        println!("Cache cleared");

        c.bench_function("get_one:random", |b| {
            //clear_cache(true).expect("Failed to clear fs cache");
            //println!("Cache cleared");

            let mut rng = thread_rng();
            // Generate a large number of random values up front
            let random_values: Vec<i128> =
                (0..SCALE).map(|_| rng.gen_range(0, 8192 * 8192)).collect();

            b.iter_batched_ref(
                || random_values.clone(), // Setup: clone the pre-generated vector (cheap compared to generation)
                |random_values| {
                    for &val in random_values.iter() {
                        let _result = get_one
                            .execute_apply(&[Value::Int(val)], &mut env)
                            .expect("Function call failed");
                    }
                },
                criterion::BatchSize::SmallInput, // Choose an appropriate batch size
            )
        });
    }

    global_context.commit().unwrap();
}

criterion_group! {
    name = benches;
    config = {
        if cfg!(feature = "flamegraph") {
            Criterion::default().with_profiler(PProfProfiler::new(100, Output::Flamegraph(None)))
        } else if cfg!(feature = "pb") {
            Criterion::default().with_profiler(PProfProfiler::new(100, Output::Protobuf))
        } else {
            Criterion::default()
        }
    };
    targets = read_bench_sequential, read_bench_random
}

criterion_main!(benches);
