use ic_config::embedders::{Config, PersistenceType};
use ic_embedders::{wasmtime_embedder::WasmtimeInstance, InstanceRunResult, WasmtimeEmbedder};
use ic_interfaces::execution_environment::{ExecutionParameters, SubnetAvailableMemory};
use ic_logger::{replica_logger::no_op_logger, ReplicaLogger};
use ic_registry_routing_table::{CanisterIdRange, RoutingTable};
use ic_registry_subnet_type::SubnetType;
use ic_replicated_state::NumWasmPages;
use ic_sys::PAGE_SIZE;
use ic_system_api::{ApiType, SystemApiImpl};
use ic_test_utilities::{
    cycles_account_manager::CyclesAccountManagerBuilder,
    mock_time,
    state::SystemStateBuilder,
    types::ids::{call_context_test_id, subnet_test_id, user_test_id},
    with_test_replica_logger,
};
use ic_types::{
    methods::{FuncRef, WasmMethod},
    CanisterId, ComputeAllocation, Cycles, NumBytes, NumInstructions, PrincipalId,
};
use ic_wasm_types::BinaryEncodedWasm;
use ic_wasm_utils::instrumentation::{instrument, InstructionCostTable};
use lazy_static::lazy_static;
use maplit::btreemap;
use proptest::prelude::*;
use std::collections::BTreeSet;
use std::sync::Arc;

const MAX_NUM_INSTRUCTIONS: NumInstructions = NumInstructions::new(1_000_000_000);

lazy_static! {
    static ref MAX_SUBNET_AVAILABLE_MEMORY: SubnetAvailableMemory =
        SubnetAvailableMemory::new(NumBytes::new(std::u64::MAX));
}

fn test_api_for_update(
    log: ReplicaLogger,
    caller: Option<PrincipalId>,
    payload: Vec<u8>,
    subnet_type: SubnetType,
) -> SystemApiImpl<ic_system_api::SystemStateAccessorDirect> {
    let caller = caller.unwrap_or_else(|| user_test_id(24).get());
    let subnet_id = subnet_test_id(1);
    let routing_table = Arc::new(RoutingTable::new(btreemap! {
        CanisterIdRange{ start: CanisterId::from(0), end: CanisterId::from(0xff) } => subnet_id,
    }));
    let subnet_records = Arc::new(btreemap! {
        subnet_id => subnet_type,
    });
    let system_state = SystemStateBuilder::default().build();
    let cycles_account_manager = Arc::new(
        CyclesAccountManagerBuilder::new()
            .with_subnet_type(subnet_type)
            .build(),
    );
    let canister_memory_limit = NumBytes::from(4 << 30);
    let canister_current_memory_usage = NumBytes::from(0);

    let system_state_accessor =
        ic_system_api::SystemStateAccessorDirect::new(system_state, cycles_account_manager);
    SystemApiImpl::new(
        ApiType::update(
            mock_time(),
            payload,
            Cycles::from(0),
            caller,
            call_context_test_id(13),
            subnet_id,
            subnet_type,
            routing_table,
            subnet_records,
        ),
        system_state_accessor,
        canister_current_memory_usage,
        ExecutionParameters {
            instruction_limit: MAX_NUM_INSTRUCTIONS,
            canister_memory_limit,
            subnet_available_memory: MAX_SUBNET_AVAILABLE_MEMORY.clone(),
            compute_allocation: ComputeAllocation::default(),
        },
        log,
    )
}

fn make_module_wat(heap_size: usize) -> String {
    format!(
        r#"
    (module
      (import "ic0" "msg_reply" (func $msg_reply))
      (import "ic0" "msg_reply_data_append"
        (func $msg_reply_data_append (param i32) (param i32)))
      (import "ic0" "msg_arg_data_copy"
        (func $ic0_msg_arg_data_copy (param i32) (param i32) (param i32)))
      (import "ic0" "msg_arg_data_size"
        (func $ic0_msg_arg_data_size (result i32)))

      (func $dump_heap
        (call $msg_reply_data_append (i32.const 0) (i32.mul (memory.size) (i32.const 0x10000)))
        (call $msg_reply)
      )

      ;; write to memory
      (func $write_bytes
        ;; copy the i32 `addr` to heap[0;4]
        (call $ic0_msg_arg_data_copy
          (i32.const 0) ;; dst
          (i32.const 0) ;; off
          (i32.const 4) ;; len
        )
        ;; copy the remainder of the payload to the heap[addr;size]
        (call $ic0_msg_arg_data_copy
          ;; addr
          (i32.load (i32.const 0))
          ;; offset
          (i32.const 4)
          ;; size
          (i32.sub
            (call $ic0_msg_arg_data_size)
            (i32.const 4)
          )
        )
      )

      (memory $memory {})
      (export "memory" (memory $memory))
      (export "canister_query dump_heap" (func $dump_heap))
      (export "canister_update write_bytes" (func $write_bytes))
    )"#,
        heap_size
    )
}

#[derive(Debug, Clone)]
pub struct Write {
    dst: u32,
    bytes: Vec<u8>,
}

fn random_writes(heap_size: usize, num_writes: usize) -> impl Strategy<Value = Vec<Write>> {
    // Start generating writes at address 4096 (or higher) to avoid generating
    // writes to the first OS page. This is because we must first copy the
    // offset from the payload to Wasm memory. We store the 4-byte offset at
    // addr=0, hence dirtying the first OS page.
    let write_strategy = (4096..heap_size).prop_flat_map(move |dst| {
        let dst = dst as u32;
        // up to 128 bytes
        let remain = (heap_size - dst as usize) % 128;
        prop::collection::vec(any::<u8>(), 0..=remain).prop_map(move |bytes| Write { dst, bytes })
    });
    prop::collection::vec(write_strategy, 1..num_writes)
}

fn write_bytes(inst: &mut WasmtimeInstance, dst: u32, bytes: &[u8]) -> InstanceRunResult {
    println!(
        "write_bytes(dst: {}, page: {}, bytes: {:?})",
        dst,
        dst / *PAGE_SIZE as u32,
        bytes
    );
    let mut payload = dst.to_le_bytes().to_vec();
    payload.extend(bytes.iter());

    let mut api = test_api_for_update(no_op_logger(), None, payload, SubnetType::Application);
    inst.run(
        &mut api,
        FuncRef::Method(WasmMethod::Update("write_bytes".to_string())),
    )
    .expect("call to write_bytes failed")
}

fn buf_apply_write(heap: &mut Vec<u8>, write: &Write) {
    // match the behavior of write_bytes: copy the i32 `addr` to heap[0;4]
    heap[0..4].copy_from_slice(&write.dst.to_le_bytes());
    heap[write.dst as usize..(write.dst as usize + write.bytes.len() as usize)]
        .copy_from_slice(&write.bytes)
}

const TEST_HEAP_SIZE_BYTES: usize = WASM_PAGE_SIZE_BYTES * TEST_NUM_PAGES;
const TEST_NUM_PAGES: usize = 800;
const TEST_NUM_WRITES: usize = 2000;
const WASM_PAGE_SIZE_BYTES: usize = 65536;
const BYTES_PER_INSTRUCTION: usize = 1;

fn wat2wasm(wat: &str) -> Result<BinaryEncodedWasm, wabt::Error> {
    wabt::wat2wasm(wat).map(BinaryEncodedWasm::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    use ic_embedders::wasm_executor::compute_page_delta;
    // Get .current() trait method
    use ic_interfaces::execution_environment::HypervisorError;
    use ic_logger::ReplicaLogger;
    use ic_replicated_state::{PageIndex, PageMap};
    use ic_test_utilities::types::ids::canister_test_id;
    use memory_tracker::DirtyPageTracking;
    use proptest::strategy::ValueTree;

    fn apply_writes_and_check_heap(writes: Vec<Write>, dirty_page_tracking: DirtyPageTracking) {
        with_test_replica_logger(|log| {
            let wat = make_module_wat(TEST_NUM_PAGES);
            let wasm = wat2wasm(&wat).unwrap();

            let output_instrumentation = instrument(&wasm, &InstructionCostTable::new()).unwrap();

            // We will perform identical writes to wasm module's heap and this buffer.
            let mut test_heap = vec![0; TEST_HEAP_SIZE_BYTES];
            // Use SIGSEGV tracking and later compare against /proc/pic/pagemap.
            let config = Config {
                persistence_type: PersistenceType::Sigsegv,
                ..Default::default()
            };
            let embedder = WasmtimeEmbedder::new(config, log);
            let embedder_cache = embedder
                .compile(PersistenceType::Sigsegv, &output_instrumentation.binary)
                .unwrap();
            let mut page_map = PageMap::default();
            let mut dirty_pages: BTreeSet<u64> = BTreeSet::new();

            for write in &writes {
                let mut instance = embedder.new_instance(
                    canister_test_id(1),
                    &embedder_cache,
                    &[],
                    NumWasmPages::from(0),
                    None,
                    Some(page_map.clone()),
                    dirty_page_tracking,
                );
                instance.set_num_instructions(MAX_NUM_INSTRUCTIONS);

                // Apply the write to the test buffer.
                buf_apply_write(&mut test_heap, write);

                // Apply the write to the Wasm instance.
                let result = write_bytes(&mut instance, write.dst, &write.bytes);

                // Compare the written regions.
                let wasm_heap: &[u8] = unsafe {
                    let addr = instance.heap_addr();
                    let size_in_bytes = instance.heap_size().get() as usize * WASM_PAGE_SIZE_BYTES;
                    std::slice::from_raw_parts_mut(addr as *mut _, size_in_bytes)
                };
                let start = write.dst as usize;
                let end = start + write.bytes.len();
                assert_eq!(wasm_heap[start..end], test_heap[start..end]);

                if dirty_page_tracking == DirtyPageTracking::Track {
                    dirty_pages.extend(result.dirty_pages.iter().map(|x| x.get()));

                    // Verify that wasm heap and test buffer are the same.
                    let i = result.dirty_pages.last().unwrap().get();
                    let offset = i as usize * *PAGE_SIZE as usize;
                    let page1 = unsafe { test_heap.as_ptr().add(offset) };
                    let page2 = unsafe { wasm_heap.as_ptr().add(offset) };
                    let pages_match = unsafe {
                        libc::memcmp(
                            page1 as *const libc::c_void,
                            page2 as *const libc::c_void,
                            *PAGE_SIZE,
                        )
                    };
                    assert!(
                        pages_match == 0,
                        "page({}) of test buffer and Wasm heap doesn't match",
                        i
                    );
                    page_map.update(compute_page_delta(&instance, &result.dirty_pages));
                }
            }

            if dirty_page_tracking == DirtyPageTracking::Track {
                for i in 0..TEST_NUM_PAGES {
                    let wasm_page = page_map.get_page(PageIndex::new(i as u64));
                    let test_page = &test_heap[i * *PAGE_SIZE..(i + 1) * *PAGE_SIZE];
                    assert_eq!(wasm_page[..], test_page[..]);
                }

                let sigsegv_dirty_pages = dirty_pages.iter().cloned().collect::<Vec<u64>>();

                let writes_pages: Vec<u64> = {
                    let mut result = BTreeSet::new();
                    // Pre-populate with page(0). This is because despite 0 does
                    // not appear in any writes, calling $write_bytes dirties
                    // page(0) by copying the 4-byte value to addr=0.
                    result.insert(0);
                    // Add the target pages.
                    result.extend(writes.iter().map(|w| w.dst as u64 / *PAGE_SIZE as u64));
                    result.iter().cloned().collect()
                };

                // Check SIGSEGV against expected.
                assert_eq!(
                sigsegv_dirty_pages,
                writes_pages,
                "dirty pages returned by SIGSEGV tracking (left) don't match the expected value (right)"
            );
            }
        });
    }

    fn random_payload() -> Vec<u8> {
        let mut runner = proptest::test_runner::TestRunner::deterministic();
        let writes: Vec<Write> = random_writes(TEST_HEAP_SIZE_BYTES, TEST_NUM_WRITES)
            .new_tree(&mut runner)
            .unwrap()
            .current()
            .iter()
            .filter(|w| !w.bytes.is_empty())
            .cloned()
            .collect();

        let mut payload: Vec<u8> = vec![];
        for w in &writes {
            payload.extend(&w.bytes);
        }
        payload
    }

    #[test]
    fn test_running_out_of_instructions() {
        with_test_replica_logger(|log| {
            let subnet_type = SubnetType::Application;

            let dst: u32 = 0;
            let mut payload: Vec<u8> = dst.to_le_bytes().to_vec();
            payload.extend(random_payload());

            // Set maximum number of instructions to some low value to trap
            let max_num_instructions = NumInstructions::new(100);

            // Consumes less than max_num_instructions.
            let instructions_consumed_without_data = get_num_instructions_consumed(
                log.clone(),
                dst.to_le_bytes().to_vec(),
                max_num_instructions,
                subnet_type,
            )
            .unwrap();
            assert!(instructions_consumed_without_data.get() > 0);

            // Exceeds the maximum amount of instructions.
            assert_eq!(
                get_num_instructions_consumed(log, payload, max_num_instructions, subnet_type,),
                Err(HypervisorError::OutOfInstructions)
            )
        })
    }

    #[test]
    fn test_proportional_instructions_consumption_to_data_size() {
        with_test_replica_logger(|log| {
            let subnet_type = SubnetType::Application;
            let dst: u32 = 0;

            let mut payload: Vec<u8> = dst.to_le_bytes().to_vec();
            payload.extend(random_payload());
            let payload_size = payload.len() - 4;

            let mut double_size_payload: Vec<u8> = payload.clone();
            double_size_payload.extend(random_payload());

            let instructions_consumed_without_data = get_num_instructions_consumed(
                log.clone(),
                dst.to_le_bytes().to_vec(),
                MAX_NUM_INSTRUCTIONS,
                subnet_type,
            )
            .unwrap();

            {
                // Number of instructions consumed only for copying the payload.
                let consumed_instructions = get_num_instructions_consumed(
                    log.clone(),
                    payload,
                    MAX_NUM_INSTRUCTIONS,
                    subnet_type,
                )
                .unwrap()
                    - instructions_consumed_without_data;
                assert_eq!(
                    consumed_instructions.get(),
                    (payload_size / BYTES_PER_INSTRUCTION) as u64
                );
            }

            {
                // Number of instructions consumed increased with the size of the data.
                let consumed_instructions = get_num_instructions_consumed(
                    log,
                    double_size_payload,
                    MAX_NUM_INSTRUCTIONS,
                    subnet_type,
                )
                .unwrap()
                    - instructions_consumed_without_data;

                assert_eq!(
                    consumed_instructions.get(),
                    (2 * payload_size / BYTES_PER_INSTRUCTION) as u64
                );
            }
        })
    }

    #[test]
    fn test_no_instructions_consumption_based_on_data_size_on_system_subnet() {
        with_test_replica_logger(|log| {
            let subnet_type = SubnetType::System;
            let dst: u32 = 0;

            let mut payload: Vec<u8> = dst.to_le_bytes().to_vec();
            payload.extend(random_payload());

            let mut double_size_payload: Vec<u8> = payload.clone();
            double_size_payload.extend(random_payload());

            let instructions_consumed_without_data = get_num_instructions_consumed(
                log.clone(),
                dst.to_le_bytes().to_vec(),
                MAX_NUM_INSTRUCTIONS,
                subnet_type,
            )
            .unwrap();

            {
                // Number of instructions consumed for copying the payload is zero.
                let consumed_instructions = get_num_instructions_consumed(
                    log.clone(),
                    payload,
                    MAX_NUM_INSTRUCTIONS,
                    subnet_type,
                )
                .unwrap()
                    - instructions_consumed_without_data;
                assert_eq!(consumed_instructions.get(), 0);
            }

            {
                // Number of instructions consumed for copying the payload is zero.
                let consumed_instructions = get_num_instructions_consumed(
                    log,
                    double_size_payload,
                    MAX_NUM_INSTRUCTIONS,
                    subnet_type,
                )
                .unwrap()
                    - instructions_consumed_without_data;
                assert_eq!(consumed_instructions.get(), 0);
            }
        })
    }

    fn get_num_instructions_consumed(
        log: ReplicaLogger,
        payload: Vec<u8>,
        max_num_instructions: NumInstructions,
        subnet_type: SubnetType,
    ) -> Result<NumInstructions, HypervisorError> {
        let wat = make_module_wat(2 * TEST_NUM_PAGES);
        let wasm = wat2wasm(&wat).unwrap();

        let config = Config {
            persistence_type: PersistenceType::Sigsegv,
            ..Default::default()
        };

        let embedder = WasmtimeEmbedder::new(config, log.clone());
        let output_instrumentation = instrument(&wasm, &InstructionCostTable::new()).unwrap();
        let mut inst = embedder.new_instance(
            canister_test_id(1),
            &embedder
                .compile(PersistenceType::Sigsegv, &output_instrumentation.binary)
                .unwrap(),
            &[],
            NumWasmPages::from(0),
            None,
            Some(PageMap::default()),
            DirtyPageTracking::Track,
        );
        inst.set_num_instructions(max_num_instructions);

        let mut api = test_api_for_update(log, None, payload, subnet_type);
        inst.run(
            &mut api,
            FuncRef::Method(WasmMethod::Update("write_bytes".to_string())),
        )?;

        // The amount of instructions consumed.
        Ok(max_num_instructions - inst.get_num_instructions())
    }

    #[test]
    fn wasmtime_random_memory_writes() {
        // The seed value will always be the same for a particular version of
        // Proptest and algorithm, but may change across releases.
        let mut runner = proptest::test_runner::TestRunner::deterministic();
        // Random, *non-empty* writes
        let writes: Vec<Write> = random_writes(TEST_HEAP_SIZE_BYTES, TEST_NUM_WRITES)
            .new_tree(&mut runner)
            .unwrap()
            .current()
            .iter()
            .filter(|w| !w.bytes.is_empty())
            .cloned()
            .collect();
        apply_writes_and_check_heap(writes, DirtyPageTracking::Track);
    }

    #[test]
    fn wasmtime_random_memory_writes_ignore_dirty_pages() {
        // The seed value will always be the same for a particular version of
        // Proptest and algorithm, but may change across releases.
        let mut runner = proptest::test_runner::TestRunner::deterministic();
        // Random, *non-empty* writes
        let writes: Vec<Write> = random_writes(TEST_HEAP_SIZE_BYTES, TEST_NUM_WRITES)
            .new_tree(&mut runner)
            .unwrap()
            .current()
            .iter()
            .filter(|w| !w.bytes.is_empty())
            .cloned()
            .collect();
        apply_writes_and_check_heap(writes, DirtyPageTracking::Ignore);
    }
}
