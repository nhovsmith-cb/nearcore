use crate::bandwidth_scheduler::BandwidthSchedulerOutput;
use crate::config::{
    safe_add_gas, total_prepaid_exec_fees, total_prepaid_gas, total_prepaid_send_fees,
};
use crate::ApplyState;
use bytesize::ByteSize;
use near_parameters::{ActionCosts, RuntimeConfig};
use near_primitives::bandwidth_scheduler::{
    BandwidthRequest, BandwidthRequests, BandwidthRequestsV1, BandwidthSchedulerParams,
};
use near_primitives::congestion_info::{CongestionControl, CongestionInfo, CongestionInfoV1};
use near_primitives::errors::{IntegerOverflowError, RuntimeError};
use near_primitives::receipt::{
    Receipt, ReceiptEnum, ReceiptOrStateStoredReceipt, StateStoredReceipt,
    StateStoredReceiptMetadata,
};
use near_primitives::types::{EpochInfoProvider, Gas, ShardId};
use near_primitives::version::ProtocolFeature;
use near_store::trie::outgoing_metadata::{OutgoingMetadatas, ReceiptGroupsConfig};
use near_store::trie::receipts_column_helper::{
    DelayedReceiptQueue, ShardsOutgoingReceiptBuffer, TrieQueue, TrieQueueIterator,
};
use near_store::{StorageError, TrieAccess, TrieUpdate};
use near_vm_runner::logic::ProtocolVersion;
use std::borrow::Cow;
use std::collections::HashMap;

/// Handle receipt forwarding for different protocol versions.
pub(crate) enum ReceiptSink {
    V1(ReceiptSinkV1),
    V2(ReceiptSinkV2),
}

/// Always put receipt to the outgoing receipts.
pub(crate) struct ReceiptSinkV1 {
    pub(crate) outgoing_receipts: Vec<Receipt>,
}

/// A helper struct to buffer or forward receipts.
///
/// When the runtime produces receipts for other shards, a `ReceiptSink` accept
/// them and either puts them in the outgoing receipts list, or puts them in a
/// buffer which is stored in the trie.
///
/// This is for congestion control, allowing to apply backpressure from the
/// receiving shard and stopping us from sending more receipts to it than its
/// nodes can keep in memory.
pub(crate) struct ReceiptSinkV2 {
    /// Keeps track of the local shard's congestion info while adding and
    /// removing buffered or delayed receipts. At the end of applying receipts,
    /// it will be a field in the [`ApplyResult`]. For this chunk, it is not
    /// used to make forwarding decisions.
    pub(crate) own_congestion_info: CongestionInfo,
    pub(crate) outgoing_receipts: Vec<Receipt>,
    pub(crate) outgoing_limit: HashMap<ShardId, OutgoingLimit>,
    pub(crate) outgoing_buffers: ShardsOutgoingReceiptBuffer,
    pub(crate) outgoing_metadatas: OutgoingMetadatas,
    pub(crate) bandwidth_scheduler_output: Option<BandwidthSchedulerOutput>,
    pub(crate) protocol_version: ProtocolVersion,
}

/// Limits for outgoing receipts to a shard.
/// Receipts are sent out until the limit is hit, after that they're buffered.
pub(crate) struct OutgoingLimit {
    pub gas: Gas,
    pub size: u64,
}

#[allow(clippy::large_enum_variant)]
enum ReceiptForwarding {
    Forwarded,
    NotForwarded(Receipt),
}

/// A wrapper around `DelayedReceiptQueue` to accumulate changes in gas and
/// bytes.
///
/// This struct exists for two reasons. One, to encapsulate the accounting of
/// gas and bytes in functions that can be called in all necessary places. Two,
/// to accumulate changes and only apply them to `CongestionInfo` in the end,
/// which avoids problems with multiple mutable borrows with the closure
/// approach we currently have in receipt processing code.
///
/// We use positive added and removed values to avoid integer conversions with
/// the associated additional overflow conditions.
pub(crate) struct DelayedReceiptQueueWrapper {
    queue: DelayedReceiptQueue,
    new_delayed_gas: Gas,
    new_delayed_bytes: u64,
    removed_delayed_gas: Gas,
    removed_delayed_bytes: u64,
}

impl ReceiptSink {
    pub(crate) fn new(
        protocol_version: ProtocolVersion,
        trie: &dyn TrieAccess,
        apply_state: &ApplyState,
        prev_own_congestion_info: Option<CongestionInfo>,
        bandwidth_scheduler_output: Option<BandwidthSchedulerOutput>,
    ) -> Result<Self, StorageError> {
        if let Some(own_congestion_info) = prev_own_congestion_info {
            debug_assert!(ProtocolFeature::CongestionControl.enabled(protocol_version));
            let outgoing_buffers = ShardsOutgoingReceiptBuffer::load(trie)?;

            let outgoing_limit: HashMap<ShardId, OutgoingLimit> = apply_state
                .congestion_info
                .iter()
                .map(|(&shard_id, congestion)| {
                    let other_congestion_control = CongestionControl::new(
                        apply_state.config.congestion_control_config,
                        congestion.congestion_info,
                        congestion.missed_chunks_count,
                    );
                    let gas_limit = if shard_id != apply_state.shard_id {
                        other_congestion_control.outgoing_gas_limit(apply_state.shard_id)
                    } else {
                        // No gas limits on receipts that stay on the same shard. Backpressure
                        // wouldn't help, the receipt takes the same memory if buffered or
                        // in the delayed receipts queue.
                        Gas::MAX
                    };

                    let size_limit =
                        other_congestion_control.outgoing_size_limit(apply_state.shard_id);

                    (shard_id, OutgoingLimit { gas: gas_limit, size: size_limit })
                })
                .collect();

            let outgoing_metadatas = OutgoingMetadatas::load(
                trie,
                outgoing_buffers.shards(),
                ReceiptGroupsConfig::default_config(),
                apply_state.current_protocol_version,
            )?;

            Ok(ReceiptSink::V2(ReceiptSinkV2 {
                own_congestion_info,
                outgoing_receipts: Vec::new(),
                outgoing_limit,
                outgoing_buffers,
                outgoing_metadatas,
                bandwidth_scheduler_output,
                protocol_version,
            }))
        } else {
            debug_assert!(!ProtocolFeature::CongestionControl.enabled(protocol_version));
            Ok(ReceiptSink::V1(ReceiptSinkV1 { outgoing_receipts: Vec::new() }))
        }
    }

    /// Forward receipts already in the buffer to the outgoing receipts vector, as
    /// much as the gas limits allow.
    pub(crate) fn forward_from_buffer(
        &mut self,
        state_update: &mut TrieUpdate,
        apply_state: &ApplyState,
    ) -> Result<(), RuntimeError> {
        match self {
            ReceiptSink::V1(_inner) => Ok(()),
            ReceiptSink::V2(inner) => inner.forward_from_buffer(state_update, apply_state),
        }
    }

    /// Put a receipt in the outgoing receipts vector (=forward) if the
    /// congestion preventing limits allow it. Put it in the buffered receipts
    /// queue otherwise.
    pub(crate) fn forward_or_buffer_receipt(
        &mut self,
        receipt: Receipt,
        apply_state: &ApplyState,
        state_update: &mut TrieUpdate,
        epoch_info_provider: &dyn EpochInfoProvider,
    ) -> Result<(), RuntimeError> {
        match self {
            ReceiptSink::V1(inner) => {
                inner.forward(receipt);
                Ok(())
            }
            ReceiptSink::V2(inner) => inner.forward_or_buffer_receipt(
                receipt,
                apply_state,
                state_update,
                epoch_info_provider,
            ),
        }
    }

    pub(crate) fn outgoing_receipts(&self) -> &[Receipt] {
        match self {
            ReceiptSink::V1(inner) => &inner.outgoing_receipts,
            ReceiptSink::V2(inner) => &inner.outgoing_receipts,
        }
    }

    pub(crate) fn into_outgoing_receipts(self) -> Vec<Receipt> {
        match self {
            ReceiptSink::V1(inner) => inner.outgoing_receipts,
            ReceiptSink::V2(inner) => inner.outgoing_receipts,
        }
    }

    pub(crate) fn own_congestion_info(&self) -> Option<CongestionInfo> {
        match self {
            ReceiptSink::V1(_) => None,
            ReceiptSink::V2(inner) => Some(inner.own_congestion_info),
        }
    }

    pub(crate) fn bandwidth_scheduler_output(&self) -> Option<&BandwidthSchedulerOutput> {
        match self {
            ReceiptSink::V1(_) => None,
            ReceiptSink::V2(inner) => inner.bandwidth_scheduler_output.as_ref(),
        }
    }

    /// Generate bandwidth requests based on the receipts stored in the outgoing buffers.
    pub(crate) fn generate_bandwidth_requests(
        &self,
        trie: &dyn TrieAccess,
        side_effects: bool,
    ) -> Result<Option<BandwidthRequests>, StorageError> {
        match self {
            ReceiptSink::V1(_) => Ok(None),
            ReceiptSink::V2(inner) => inner.generate_bandwidth_requests(trie, side_effects),
        }
    }
}

impl ReceiptSinkV1 {
    /// V1 can only forward receipts.
    pub(crate) fn forward(&mut self, receipt: Receipt) {
        self.outgoing_receipts.push(receipt);
    }
}

impl ReceiptSinkV2 {
    /// Forward receipts already in the buffer to the outgoing receipts vector, as
    /// much as the gas limits allow.
    pub(crate) fn forward_from_buffer(
        &mut self,
        state_update: &mut TrieUpdate,
        apply_state: &ApplyState,
    ) -> Result<(), RuntimeError> {
        // store shards in vec to avoid borrowing self.outgoing_limit
        let shards: Vec<_> = self.outgoing_limit.keys().copied().collect();
        for shard_id in shards {
            self.forward_from_buffer_to_shard(shard_id, state_update, apply_state)?;
        }
        Ok(())
    }

    fn forward_from_buffer_to_shard(
        &mut self,
        shard_id: ShardId,
        state_update: &mut TrieUpdate,
        apply_state: &ApplyState,
    ) -> Result<(), RuntimeError> {
        let mut num_forwarded = 0;
        let mut outgoing_metadatas_updates: Vec<(ByteSize, Gas)> = Vec::new();
        for receipt_result in
            self.outgoing_buffers.to_shard(shard_id).iter(&state_update.trie, true)
        {
            let receipt = receipt_result?;
            let gas = receipt_congestion_gas(&receipt, &apply_state.config)?;
            let size = receipt_size(&receipt)?;
            let should_update_outgoing_metadatas = receipt.should_update_outgoing_metadatas();
            let receipt = receipt.into_receipt();

            match Self::try_forward(
                receipt,
                gas,
                size,
                shard_id,
                &mut self.outgoing_limit,
                &mut self.outgoing_receipts,
                apply_state,
            )? {
                ReceiptForwarding::Forwarded => {
                    self.own_congestion_info.remove_receipt_bytes(size)?;
                    self.own_congestion_info.remove_buffered_receipt_gas(gas)?;
                    if should_update_outgoing_metadatas {
                        // Can't update metadatas immediately because state_update is borrowed by iterator.
                        outgoing_metadatas_updates.push((ByteSize::b(size), gas));
                    }
                    // count how many to release later to avoid modifying
                    // `state_update` while iterating based on
                    // `state_update.trie`.
                    num_forwarded += 1;
                }
                ReceiptForwarding::NotForwarded(_) => {
                    break;
                }
            }
        }
        self.outgoing_buffers.to_shard(shard_id).pop_n(state_update, num_forwarded)?;
        for (size, gas) in outgoing_metadatas_updates {
            self.outgoing_metadatas.update_on_receipt_popped(shard_id, size, gas, state_update)?;
        }
        Ok(())
    }

    /// Put a receipt in the outgoing receipts vector (=forward) if the
    /// congestion preventing limits allow it. Put it in the buffered receipts
    /// queue otherwise.
    pub(crate) fn forward_or_buffer_receipt(
        &mut self,
        receipt: Receipt,
        apply_state: &ApplyState,
        state_update: &mut TrieUpdate,
        epoch_info_provider: &dyn EpochInfoProvider,
    ) -> Result<(), RuntimeError> {
        let shard = epoch_info_provider
            .account_id_to_shard_id(receipt.receiver_id(), &apply_state.epoch_id)?;

        let size = compute_receipt_size(&receipt)?;
        let gas = compute_receipt_congestion_gas(&receipt, &apply_state.config)?;

        match Self::try_forward(
            receipt,
            gas,
            size,
            shard,
            &mut self.outgoing_limit,
            &mut self.outgoing_receipts,
            apply_state,
        )? {
            ReceiptForwarding::Forwarded => (),
            ReceiptForwarding::NotForwarded(receipt) => {
                self.buffer_receipt(
                    receipt,
                    size,
                    gas,
                    state_update,
                    shard,
                    apply_state.config.use_state_stored_receipt,
                )?;
            }
        }
        Ok(())
    }

    /// Forward a receipt if possible and return whether it was forwarded or
    /// not.
    ///
    /// This does not take `&mut self` as first argument to make lifetime
    /// management easier. Instead it takes exactly the fields it requires,
    /// namely `outgoing_limit` and `outgoing_receipt`.
    fn try_forward(
        receipt: Receipt,
        gas: u64,
        size: u64,
        shard: ShardId,
        outgoing_limit: &mut HashMap<ShardId, OutgoingLimit>,
        outgoing_receipts: &mut Vec<Receipt>,
        apply_state: &ApplyState,
    ) -> Result<ReceiptForwarding, RuntimeError> {
        // Default case set to `Gas::MAX`: If no outgoing limit was defined for the receiving
        // shard, this usually just means the feature is not enabled. Or, it
        // could be a special case during resharding events. Or even a bug. In
        // any case, if we cannot know a limit, treating it as literally "no
        // limit" is the safest approach to ensure availability.
        // For the size limit, we default to the usual limit that is applied to all (non-special) shards.
        let default_outgoing_limit = OutgoingLimit {
            gas: Gas::MAX,
            size: apply_state.config.congestion_control_config.outgoing_receipts_usual_size_limit,
        };
        let forward_limit = outgoing_limit.entry(shard).or_insert(default_outgoing_limit);

        if forward_limit.gas > gas && forward_limit.size > size {
            outgoing_receipts.push(receipt);
            // underflow impossible: checked forward_limit > gas/size_to_forward above
            forward_limit.gas -= gas;
            forward_limit.size -= size;
            Ok(ReceiptForwarding::Forwarded)
        } else {
            Ok(ReceiptForwarding::NotForwarded(receipt))
        }
    }

    /// Put a receipt in the outgoing receipt buffer of a shard.
    fn buffer_receipt(
        &mut self,
        receipt: Receipt,
        size: u64,
        gas: u64,
        state_update: &mut TrieUpdate,
        shard: ShardId,
        use_state_stored_receipt: bool,
    ) -> Result<(), RuntimeError> {
        let receipt = match use_state_stored_receipt {
            true => {
                let metadata =
                    StateStoredReceiptMetadata { congestion_gas: gas, congestion_size: size };
                let receipt =
                    StateStoredReceipt::new_owned(receipt, metadata, self.protocol_version);
                let receipt = ReceiptOrStateStoredReceipt::StateStoredReceipt(receipt);
                receipt
            }
            false => ReceiptOrStateStoredReceipt::Receipt(std::borrow::Cow::Owned(receipt)),
        };

        self.own_congestion_info.add_receipt_bytes(size)?;
        self.own_congestion_info.add_buffered_receipt_gas(gas)?;

        if receipt.should_update_outgoing_metadatas() {
            self.outgoing_metadatas.update_on_receipt_pushed(
                shard,
                ByteSize::b(size),
                gas,
                state_update,
            )?;
        }

        self.outgoing_buffers.to_shard(shard).push_back(state_update, &receipt)?;
        Ok(())
    }

    fn generate_bandwidth_requests(
        &self,
        trie: &dyn TrieAccess,
        side_effects: bool,
    ) -> Result<Option<BandwidthRequests>, StorageError> {
        if !ProtocolFeature::BandwidthScheduler.enabled(self.protocol_version) {
            return Ok(None);
        }

        let params = self
            .bandwidth_scheduler_output
            .as_ref()
            .expect("BandwidthScheduler is enabled and should produce params")
            .params;

        let mut requests = Vec::new();
        for shard_id in self.outgoing_buffers.shards() {
            if let Some(request) =
                self.generate_bandwidth_request(shard_id, trie, side_effects, &params)?
            {
                requests.push(request);
            }
        }

        Ok(Some(BandwidthRequests::V1(BandwidthRequestsV1 { requests })))
    }

    fn generate_bandwidth_request(
        &self,
        to_shard: ShardId,
        trie: &dyn TrieAccess,
        side_effects: bool,
        params: &BandwidthSchedulerParams,
    ) -> Result<Option<BandwidthRequest>, StorageError> {
        let outgoing_receipts_buffer_len = self.outgoing_buffers.buffer_len(to_shard).unwrap_or(0);

        if outgoing_receipts_buffer_len == 0 {
            // No receipts in the outgoing buffer, nothing to request.
            return Ok(None);
        };

        // To make a proper bandwidth request we need the metadata for the outgoing buffer to be fully initialized
        // (i.e. contain data about all of the receipts in the outgoing buffer). There is a moment right after the
        // protocol upgrade where the outgoing buffer contains receipts which were buffered in the previous protocol
        // version where metadata was not enabled. Metadata doesn't contain information about them.
        // We can't make a proper request in this case, so we make a basic request while we wait for
        // metadata to become fully initialized. The basic request requests just `max_receipt_size`. This is enough to
        // ensure liveness, as all receipts are smaller than `max_receipt_size`. The resulting behaviour is similar
        // to the previous approach where the `allowed_shard` was assigned most of the bandwidth.
        // Over time these old receipts will be removed from the outgoing buffer and eventually metadata will contain
        // information about every receipt in the buffer. From that point on we will be able to make
        // proper bandwidth requests.
        let Some(metadata) = self.outgoing_metadatas.get_metadata_for_shard(&to_shard) else {
            // Metadata not ready, make a basic request.
            return Ok(Some(BandwidthRequest::make_max_receipt_size_request(to_shard, params)));
        };

        if metadata.total_receipts_num() != outgoing_receipts_buffer_len {
            // Metadata not ready, contains less receipts than the outgoing buffer. Make a basic request.
            return Ok(Some(BandwidthRequest::make_max_receipt_size_request(to_shard, params)));
        }

        // Metadata is fully initialized, make a proper bandwidth request using it.
        let receipt_sizes_iter = metadata.iter_receipt_group_sizes(trie, side_effects);
        BandwidthRequest::make_from_receipt_sizes(to_shard, receipt_sizes_iter, params)
    }
}

/// Get the receipt gas from the receipt that was retrieved from the state.
/// If it is a [Receipt], the gas will be computed.
/// If it s the [StateStoredReceipt], the size will be read from the metadata.
pub(crate) fn receipt_congestion_gas(
    receipt: &ReceiptOrStateStoredReceipt,
    config: &RuntimeConfig,
) -> Result<Gas, IntegerOverflowError> {
    match receipt {
        ReceiptOrStateStoredReceipt::Receipt(receipt) => {
            compute_receipt_congestion_gas(receipt, config)
        }
        ReceiptOrStateStoredReceipt::StateStoredReceipt(receipt) => {
            Ok(receipt.metadata().congestion_gas)
        }
    }
}

/// Calculate the gas of a receipt before it is pushed into a state queue or
/// buffer. Please note that this method should only be used when storing
/// receipts into state. It should not be used for retrieving receipts from the
/// state.
///
/// The calculation is part of protocol and should only be modified with a
/// protocol upgrade.
pub(crate) fn compute_receipt_congestion_gas(
    receipt: &Receipt,
    config: &RuntimeConfig,
) -> Result<u64, IntegerOverflowError> {
    match receipt.receipt() {
        ReceiptEnum::Action(action_receipt) => {
            // account for gas guaranteed to be used for executing the receipts
            let prepaid_exec_gas = safe_add_gas(
                total_prepaid_exec_fees(config, &action_receipt.actions, receipt.receiver_id())?,
                config.fees.fee(ActionCosts::new_action_receipt).exec_fee(),
            )?;
            // account for gas guaranteed to be used for creating new receipts
            let prepaid_send_gas = total_prepaid_send_fees(config, &action_receipt.actions)?;
            let prepaid_gas = safe_add_gas(prepaid_exec_gas, prepaid_send_gas)?;

            // account for gas potentially used for dynamic execution
            let gas_attached_to_fns = total_prepaid_gas(&action_receipt.actions)?;
            let gas = safe_add_gas(gas_attached_to_fns, prepaid_gas)?;

            Ok(gas)
        }
        ReceiptEnum::Data(_data_receipt) => {
            // Data receipts themselves don't cost gas to execute, their cost is
            // burnt at creation. What we should count, is the gas of the
            // postponed action receipt. But looking that up would require
            // reading the postponed receipt from the trie.
            // Thus, the congestion control MVP does not account for data
            // receipts or postponed receipts.
            Ok(0)
        }
        ReceiptEnum::PromiseYield(_) => {
            // The congestion control MVP does not account for yielding a
            // promise. Yielded promises are confined to a single account, hence
            // they never cross the shard boundaries. This makes it irrelevant
            // for the congestion MVP, which only counts gas in the outgoing
            // buffers and delayed receipts queue.
            Ok(0)
        }
        ReceiptEnum::PromiseResume(_) => {
            // The congestion control MVP does not account for resuming a promise.
            // Unlike `PromiseYield`, it is possible that a promise-resume ends
            // up in the delayed receipts queue.
            // But similar to a data receipt, it would be difficult to find the cost
            // of it without expensive state lookups.
            Ok(0)
        }
    }
}

/// Iterate all columns in the trie holding unprocessed receipts and
/// computes the storage consumption as well as attached gas.
///
/// This is an IO intensive operation! Only do it to bootstrap the
/// `CongestionInfo`. In normal operation, this information is kept up
/// to date and passed from chunk to chunk through chunk header fields.
pub fn bootstrap_congestion_info(
    trie: &dyn near_store::TrieAccess,
    config: &RuntimeConfig,
    shard_id: ShardId,
) -> Result<CongestionInfo, StorageError> {
    let mut receipt_bytes: u64 = 0;
    let mut delayed_receipts_gas: u128 = 0;
    let mut buffered_receipts_gas: u128 = 0;

    let delayed_receipt_queue = &DelayedReceiptQueue::load(trie)?;
    for receipt_result in delayed_receipt_queue.iter(trie, true) {
        let receipt = receipt_result?;
        let gas = receipt_congestion_gas(&receipt, config).map_err(int_overflow_to_storage_err)?;
        delayed_receipts_gas =
            safe_add_gas_to_u128(delayed_receipts_gas, gas).map_err(int_overflow_to_storage_err)?;

        let memory = receipt_size(&receipt).map_err(int_overflow_to_storage_err)? as u64;
        receipt_bytes = receipt_bytes.checked_add(memory).ok_or_else(overflow_storage_err)?;
    }

    let mut outgoing_buffers = ShardsOutgoingReceiptBuffer::load(trie)?;
    for shard in outgoing_buffers.shards() {
        for receipt_result in outgoing_buffers.to_shard(shard).iter(trie, true) {
            let receipt = receipt_result?;
            let gas =
                receipt_congestion_gas(&receipt, config).map_err(int_overflow_to_storage_err)?;
            buffered_receipts_gas = safe_add_gas_to_u128(buffered_receipts_gas, gas)
                .map_err(int_overflow_to_storage_err)?;
            let memory = receipt_size(&receipt).map_err(int_overflow_to_storage_err)? as u64;
            receipt_bytes = receipt_bytes.checked_add(memory).ok_or_else(overflow_storage_err)?;
        }
    }

    Ok(CongestionInfo::V1(CongestionInfoV1 {
        delayed_receipts_gas: delayed_receipts_gas as u128,
        buffered_receipts_gas: buffered_receipts_gas as u128,
        receipt_bytes,
        // For the first chunk, set this to the own id.
        // This allows bootstrapping without knowing all other shards.
        // It is also irrelevant, since the bootstrapped value is only used at
        // the start of applying a chunk on this shard. Other shards will only
        // see and act on the first congestion info after that.
        allowed_shard: shard_id.into(),
    }))
}

impl DelayedReceiptQueueWrapper {
    pub fn new(queue: DelayedReceiptQueue) -> Self {
        Self {
            queue,
            new_delayed_gas: 0,
            new_delayed_bytes: 0,
            removed_delayed_gas: 0,
            removed_delayed_bytes: 0,
        }
    }

    pub(crate) fn push(
        &mut self,
        trie_update: &mut TrieUpdate,
        receipt: &Receipt,
        apply_state: &ApplyState,
    ) -> Result<(), RuntimeError> {
        let config = &apply_state.config;

        let gas = compute_receipt_congestion_gas(&receipt, &config)?;
        let size = compute_receipt_size(&receipt)? as u64;

        // TODO It would be great to have this method take owned Receipt and
        // get rid of the Cow from the Receipt and StateStoredReceipt.
        let receipt = match config.use_state_stored_receipt {
            true => {
                let metadata =
                    StateStoredReceiptMetadata { congestion_gas: gas, congestion_size: size };
                let receipt = StateStoredReceipt::new_borrowed(
                    receipt,
                    metadata,
                    apply_state.current_protocol_version,
                );
                ReceiptOrStateStoredReceipt::StateStoredReceipt(receipt)
            }
            false => ReceiptOrStateStoredReceipt::Receipt(Cow::Borrowed(receipt)),
        };

        self.new_delayed_gas = safe_add_gas(self.new_delayed_gas, gas)?;
        self.new_delayed_bytes = safe_add_gas(self.new_delayed_bytes, size)?;
        self.queue.push_back(trie_update, &receipt)?;
        Ok(())
    }

    pub(crate) fn pop(
        &mut self,
        trie_update: &mut TrieUpdate,
        config: &RuntimeConfig,
    ) -> Result<Option<ReceiptOrStateStoredReceipt>, RuntimeError> {
        let receipt = self.queue.pop_front(trie_update)?;
        if let Some(receipt) = &receipt {
            let delayed_gas = receipt_congestion_gas(receipt, &config)?;
            let delayed_bytes = receipt_size(receipt)? as u64;
            self.removed_delayed_gas = safe_add_gas(self.removed_delayed_gas, delayed_gas)?;
            self.removed_delayed_bytes = safe_add_gas(self.removed_delayed_bytes, delayed_bytes)?;
        }
        Ok(receipt)
    }

    pub(crate) fn peek_iter<'a>(
        &'a self,
        trie_update: &'a TrieUpdate,
    ) -> TrieQueueIterator<'a, DelayedReceiptQueue> {
        self.queue.iter(trie_update, false)
    }

    pub(crate) fn len(&self) -> u64 {
        self.queue.len()
    }

    pub(crate) fn apply_congestion_changes(
        self,
        congestion: &mut CongestionInfo,
    ) -> Result<(), RuntimeError> {
        congestion.add_delayed_receipt_gas(self.new_delayed_gas)?;
        congestion.remove_delayed_receipt_gas(self.removed_delayed_gas)?;
        congestion.add_receipt_bytes(self.new_delayed_bytes)?;
        congestion.remove_receipt_bytes(self.removed_delayed_bytes)?;
        Ok(())
    }
}

/// Get the receipt size from the receipt that was retrieved from the state.
/// If it is a [Receipt], the size will be computed.
/// If it s the [StateStoredReceipt], the size will be read from the metadata.
pub(crate) fn receipt_size(
    receipt: &ReceiptOrStateStoredReceipt,
) -> Result<u64, IntegerOverflowError> {
    match receipt {
        ReceiptOrStateStoredReceipt::Receipt(receipt) => compute_receipt_size(receipt),
        ReceiptOrStateStoredReceipt::StateStoredReceipt(receipt) => {
            Ok(receipt.metadata().congestion_size)
        }
    }
}

/// Calculate the size of a receipt before it is pushed into a state queue or
/// buffer. Please note that this method should only be used when storing
/// receipts into state. It should not be used for retrieving receipts from the
/// state.
///
/// The calculation is part of protocol and should only be modified with a
/// protocol upgrade.
pub(crate) fn compute_receipt_size(receipt: &Receipt) -> Result<u64, IntegerOverflowError> {
    let size = borsh::object_length(&receipt).map_err(|_| IntegerOverflowError)?;
    size.try_into().map_err(|_| IntegerOverflowError)
}

fn int_overflow_to_storage_err(_err: IntegerOverflowError) -> StorageError {
    overflow_storage_err()
}

fn overflow_storage_err() -> StorageError {
    StorageError::StorageInconsistentState(
        "Calculations on stored receipt overflows calculations".to_owned(),
    )
}

// we use u128 for accumulated gas because congestion may deal with a lot of gas
fn safe_add_gas_to_u128(a: u128, b: Gas) -> Result<u128, IntegerOverflowError> {
    a.checked_add(b as u128).ok_or(IntegerOverflowError {})
}
