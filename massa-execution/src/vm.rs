use crate::error::bootstrap_file_error;
use crate::types::{
    EventStore, ExecutionContext, ExecutionData, ExecutionStep, StackElement, StepHistory,
    StepHistoryItem,
};
use crate::{
    interface_impl::InterfaceImpl,
    sce_ledger::{FinalLedger, SCELedger, SCELedgerChanges},
};
use crate::{settings::ExecutionConfigs, ExecutionError};

use massa_models::{
    api::SCELedgerInfo,
    execution::{ExecuteReadOnlyResponse, ReadOnlyResult},
    output_event::SCOutputEvent,
    prehash::Map,
    timeslots::{get_latest_block_slot_at_timestamp, slot_count_in_range},
    Address, Amount, BlockId, OperationId, Slot,
};
use massa_sc_runtime::Interface;
use massa_signature::{derive_public_key, generate_random_private_key};
use massa_time::MassaTime;
use rand::SeedableRng;
use rand_xoshiro::Xoshiro256PlusPlus;
use std::mem;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;
use tracing::debug;

/// Virtual Machine and step history system
pub(crate) struct VM {
    /// thread count
    thread_count: u8,

    genesis_timestamp: MassaTime,
    t0: MassaTime,

    /// history of SCE-active executed steps
    step_history: StepHistory,

    /// execution interface used by the runtime
    execution_interface: Box<dyn Interface>,

    /// execution context
    execution_context: Arc<Mutex<ExecutionContext>>,

    /// final events
    final_events: EventStore,
}

impl VM {
    pub fn new(
        cfg: ExecutionConfigs,
        ledger_bootstrap: Option<(SCELedger, Slot)>,
    ) -> Result<VM, ExecutionError> {
        let (ledger_bootstrap, ledger_slot) =
            if let Some((ledger_bootstrap, ledger_slot)) = ledger_bootstrap {
                // bootstrap from snapshot
                (ledger_bootstrap, ledger_slot)
            } else {
                // not bootstrapping: load initial SCE ledger from file
                let ledger_slot = Slot::new(0, cfg.thread_count.saturating_sub(1)); // last genesis block
                let ledgger_balances = serde_json::from_str::<Map<Address, Amount>>(
                    &std::fs::read_to_string(&cfg.settings.initial_sce_ledger_path)
                        .map_err(bootstrap_file_error!("loading", cfg))?,
                )
                .map_err(bootstrap_file_error!("parsing", cfg))?;
                let ledger_bootstrap = SCELedger::from_balances_map(ledgger_balances);
                (ledger_bootstrap, ledger_slot)
            };

        // Context shared between VM and the interface provided to the assembly simulator.
        let execution_context = Arc::new(Mutex::new(ExecutionContext::new(
            ledger_bootstrap,
            ledger_slot,
        )));

        // Instantiate the interface used by the assembly simulator.
        let execution_interface = Box::new(InterfaceImpl::new(
            Arc::clone(&execution_context),
            cfg.thread_count,
            cfg.t0,
            cfg.genesis_timestamp,
        ));

        Ok(VM {
            thread_count: cfg.thread_count,
            step_history: Default::default(),
            execution_interface,
            execution_context,
            final_events: Default::default(),
            genesis_timestamp: cfg.genesis_timestamp,
            t0: cfg.t0,
        })
    }

    // clone bootstrap state (final ledger and slot)
    pub fn get_bootstrap_state(&self) -> FinalLedger {
        self.execution_context
            .lock()
            .unwrap()
            .ledger_step
            .final_ledger_slot
            .clone()
    }

    /// Get events optionnally filtered by:
    /// * start slot
    /// * end slot
    /// * emitter address
    /// * original caller address
    /// * operation id
    pub fn get_filtered_sc_output_event(
        &self,
        start: Option<Slot>,
        end: Option<Slot>,
        emitter_address: Option<Address>,
        original_caller_address: Option<Address>,
        original_operation_id: Option<OperationId>,
    ) -> Vec<SCOutputEvent> {
        // iter on step history chained with final events
        let start = start.unwrap_or_else(|| Slot::new(0, 0));
        let end = end.unwrap_or(match MassaTime::now() {
            Ok(now) => get_latest_block_slot_at_timestamp(
                self.thread_count,
                self.t0,
                self.genesis_timestamp,
                now,
            )
            .unwrap_or_else(|_| Some(Slot::new(0, 0)))
            .unwrap_or_else(|| Slot::new(0, 0)),
            Err(_) => Slot::new(0, 0),
        });
        self.step_history
            .iter()
            .filter(|item| item.slot >= start && item.slot < end)
            .flat_map(|item| {
                item.events.get_filtered_sc_output_event(
                    start,
                    end,
                    emitter_address,
                    original_caller_address,
                    original_operation_id,
                )
            })
            .chain(self.final_events.get_filtered_sc_output_event(
                start,
                end,
                emitter_address,
                original_caller_address,
                original_operation_id,
            ))
            .collect()
    }

    // clone bootstrap state (final ledger and slot)
    pub fn get_sce_ledger_entry_for_addresses(
        &self,
        addresses: Vec<Address>,
    ) -> Map<Address, SCELedgerInfo> {
        let ledger = &self
            .execution_context
            .lock()
            .unwrap()
            .ledger_step
            .final_ledger_slot
            .ledger;
        addresses
            .into_iter()
            .map(|ad| {
                let entry = ledger.0.get(&ad).cloned().unwrap_or_default();
                (
                    ad,
                    SCELedgerInfo {
                        balance: entry.balance,
                        module: entry.opt_module,
                        datastore: entry.data,
                    },
                )
            })
            .collect()
    }

    /// runs an SCE-final execution step
    /// See https://github.com/massalabs/massa/wiki/vm_ledger_interaction
    ///
    /// # Parameters
    ///   * step: execution step to run
    ///   * max_final_events: max number of events kept in cache (todo should be removed when config become static)
    pub(crate) fn run_final_step(&mut self, step: ExecutionStep, max_final_events: usize) {
        // check if that step was already executed as the earliest active step
        let history_item = if let Some(cached) = self.pop_cached_step(&step) {
            // if so, pop it
            cached
        } else {
            // otherwise, clear step history an run it again explicitly
            self.step_history.clear();
            self.run_step_internal(&step)
        };

        // apply ledger changes to final ledger
        let mut context = self.execution_context.lock().unwrap();
        let mut ledger_step = &mut (*context).ledger_step;
        ledger_step
            .final_ledger_slot
            .ledger
            .apply_changes(&history_item.ledger_changes);
        ledger_step.final_ledger_slot.slot = step.slot;

        self.final_events.extend(mem::take(&mut context.events));
        self.final_events.prune(max_final_events)
    }

    /// check if step already at history front, if so, pop it
    fn pop_cached_step(&mut self, step: &ExecutionStep) -> Option<StepHistoryItem> {
        let found = if let Some(StepHistoryItem {
            slot, opt_block_id, ..
        }) = self.step_history.front()
        {
            if *slot == step.slot {
                match (&opt_block_id, &step.block) {
                    // matching miss
                    (None, None) => true,

                    // matching block
                    (Some(b_id_hist), Some((b_id_step, _b_step))) => (b_id_hist == b_id_step),

                    // miss/block mismatch
                    (None, Some(_)) => false,

                    // block/miss mismatch
                    (Some(_), None) => false,
                }
            } else {
                false // slot mismatch
            }
        } else {
            false // no item
        };

        // rerturn the step if found
        if found {
            self.step_history.pop_front()
        } else {
            None
        }
    }

    /// Tooling function that has to be run before each new step execution, even if we are in read-only
    ///
    /// Clear all caused changes in the context
    /// Set cumulative_hisory_changes = step_history.into_changes
    /// Reset the execution call stack and the owned addresses
    fn clear_and_update_context(&self) {
        let mut context = self.execution_context.lock().unwrap();
        context.ledger_step.caused_changes.clear();
        context.ledger_step.cumulative_history_changes =
            SCELedgerChanges::from(self.step_history.clone());
        context.created_addr_index = 0;
        context.created_event_index = 0;
        context.stack.clear();
        context.events.clear();
        context.read_only = false;
        context.origin_operation_id = None;
    }

    /// Prepares (updates) the shared context before the new operation.
    /// Returns a snapshot of the current caused ledger changes.
    /// See https://github.com/massalabs/massa/wiki/vm_ledger_interaction
    /// TODO: do not ignore the results
    /// TODO: consider dispatching gas fees with edorsers/endorsees as well
    /// Returns (backup of local ledger changes, backup of created_addr_index,
    /// backup of events, backup of created_events_index, backup of unsafe rng)
    fn prepare_context(
        &self,
        data: &ExecutionData,
        block_creator_addr: Address,
        block_id: BlockId,
        slot: Slot,
        operation: Option<OperationId>,
    ) -> (SCELedgerChanges, u64, EventStore, u64, Xoshiro256PlusPlus) {
        let mut context = self.execution_context.lock().unwrap();
        // make context.ledger_step credit Op's sender with Op.coins in the SCE ledger
        let _result = context
            .ledger_step
            .set_balance_delta(data.sender_address, data.coins, true);

        // make context.ledger_step credit the producer of the block B with Op.max_gas * Op.gas_price in the SCE ledger
        let _result = context.ledger_step.set_balance_delta(
            block_creator_addr,
            data.gas_price.saturating_mul_u64(data.max_gas),
            true,
        );

        // fill context for execution
        // created_addr_index is not reset here (it is used at the slot scale)
        context.gas_price = data.gas_price;
        context.max_gas = data.max_gas;
        context.stack = vec![StackElement {
            address: data.sender_address,
            coins: data.coins,
            owned_addresses: vec![data.sender_address],
        }];
        context.slot = slot;
        context.opt_block_id = Some(block_id);
        context.opt_block_creator_addr = Some(block_creator_addr);
        context.origin_operation_id = operation;

        (
            context.ledger_step.caused_changes.clone(),
            context.created_addr_index,
            context.events.clone(),
            context.created_event_index,
            context.unsafe_rng.clone(),
        )
    }

    /// Run code in read-only mode
    pub(crate) fn run_read_only(
        &self,
        slot: Slot,
        max_gas: u64,
        simulated_gas_price: Amount,
        bytecode: Vec<u8>,
        address: Option<Address>,
        result_sender: oneshot::Sender<ExecuteReadOnlyResponse>,
    ) {
        // Reset active ledger changes history
        self.clear_and_update_context();

        {
            let mut context = self.execution_context.lock().unwrap();

            // Set the call stack, using the provided address, or a random one.
            let address = address.unwrap_or_else(|| {
                let private_key = generate_random_private_key();
                let public_key = derive_public_key(&private_key);
                Address::from_public_key(&public_key)
            });

            context.stack = vec![StackElement {
                address,
                coins: Amount::zero(),
                owned_addresses: vec![address],
            }];

            // Set read-only
            context.read_only = true;

            // Set the max gas.
            context.max_gas = max_gas;

            // Set the simulated gas price.
            context.gas_price = simulated_gas_price;

            // Seed the RNG
            let mut seed: Vec<u8> = slot.to_bytes_key().to_vec();
            seed.push(0u8); // read-only
            let seed = massa_hash::hash::Hash::compute_from(&seed).to_bytes();
            context.unsafe_rng = Xoshiro256PlusPlus::from_seed(seed);
        }

        // run in the intepreter
        let run_result = massa_sc_runtime::run(&bytecode, max_gas, &*self.execution_interface);

        let mut context = self.execution_context.lock().unwrap();
        // Send result back.
        let execution_response = ExecuteReadOnlyResponse {
            executed_at: slot,
            // TODO: specify result.
            result: run_result.map_or_else(
                |_| ReadOnlyResult::Error("Failed to run in read-only mode".to_string()),
                |_| ReadOnlyResult::Ok,
            ),
            // integrate with output events.
            output_events: mem::take(&mut context.events).export(),
        };
        if result_sender.send(execution_response).is_err() {
            debug!("Execution: could not send ExecuteReadOnlyResponse.");
        }

        // Note: changes are not applied to the ledger.
    }

    /// Runs an active step
    /// See https://github.com/massalabs/massa/wiki/vm_ledger_interaction
    ///
    /// 1. Get step history (cache of final ledger changes by slot and block_id history)
    /// 2. clear caused changes
    /// 3. accumulated step history
    /// 4. Execute each block of each operation
    ///
    /// # Parameters
    ///   * step: execution step to run
    fn run_step_internal(&mut self, step: &ExecutionStep) -> StepHistoryItem {
        // reset active ledger changes history
        self.clear_and_update_context();

        {
            let mut context = self.execution_context.lock().unwrap();

            // seed the RNG
            let mut seed: Vec<u8> = step.slot.to_bytes_key().to_vec();
            seed.push(1u8); // not read-only
            if let Some((block_id, _block)) = &step.block {
                seed.extend(block_id.to_bytes()); // append block ID
            }
            let seed = massa_hash::hash::Hash::compute_from(&seed).to_bytes();
            context.unsafe_rng = Xoshiro256PlusPlus::from_seed(seed);
        }

        // run implicit and async calls
        // TODO

        // run explicit calls within the block (if the slot is not a miss)
        // note that total block gas is not checked, because currently Protocol makes the block invalid if it overflows gas
        let opt_block_id: Option<BlockId>;
        if let Some((block_id, block)) = &step.block {
            opt_block_id = Some(*block_id);

            // get block creator addr
            let block_creator_addr = Address::from_public_key(&block.header.content.creator);
            // run all operations
            for (op_idx, operation) in block.operations.iter().enumerate() {
                // process ExecuteSC operations only
                let execution_data = match ExecutionData::try_from(operation) {
                    Ok(data) => data,
                    _ => continue,
                };

                // Prepare context and save the initial ledger changes before execution.
                // The returned snapshot takes into account the initial coin credits.
                // This snapshot will be popped back if bytecode execution fails.
                let (
                    ledger_changes_backup,
                    created_addr_index_backup,
                    events_backup,
                    event_index_backup,
                    rng_backup,
                ) = self.prepare_context(
                    &execution_data,
                    block_creator_addr,
                    *block_id,
                    step.slot,
                    Some(match operation.get_operation_id() {
                        Ok(id) => id,
                        Err(_) => continue,
                    }),
                );

                // run in the intepreter
                let run_result = massa_sc_runtime::run(
                    &execution_data.bytecode,
                    execution_data.max_gas,
                    &*self.execution_interface,
                );
                if let Err(err) = run_result {
                    debug!(
                        "failed running bytecode in operation index {} in block {}: {}",
                        op_idx, block_id, err
                    );
                    // cancel the effects of execution only, pop back init_changes
                    let mut context = self.execution_context.lock().unwrap();
                    context.ledger_step.caused_changes = ledger_changes_backup;
                    context.created_addr_index = created_addr_index_backup;
                    context.events = events_backup;
                    context.created_event_index = event_index_backup;
                    context.unsafe_rng = rng_backup;
                    context.origin_operation_id = None;
                }
            }
        } else {
            // There is no block for this step, miss
            opt_block_id = None;
        }

        // generate history item
        let mut context = self.execution_context.lock().unwrap();
        StepHistoryItem {
            slot: step.slot,
            opt_block_id,
            ledger_changes: mem::take(&mut context.ledger_step.caused_changes),
            events: mem::take(&mut context.events),
        }
    }

    /// runs an SCE-active execution step
    /// See https://github.com/massalabs/massa/wiki/vm_ledger_interaction
    ///
    /// # Parameters
    ///   * step: execution step to run
    pub(crate) fn run_active_step(&mut self, step: ExecutionStep) {
        // rewind history to optimize execution
        if let Some(front_slot) = self.step_history.front().map(|h| h.slot) {
            if let Ok(len) = slot_count_in_range(front_slot, step.slot, self.thread_count) {
                self.step_history.truncate(len as usize);
            }
        }

        // run step
        let history_item = self.run_step_internal(&step);

        // push step into history
        self.step_history.push_back(history_item);
    }

    pub fn reset_to_final(&mut self) {
        self.step_history.clear();
    }
}
