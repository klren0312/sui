// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::data_fetcher::DataFetcher;
use crate::data_fetcher::RemoteFetcher;
use crate::types::*;
use futures::executor::block_on;
use move_binary_format::CompiledModule;
use move_bytecode_utils::module_cache::GetModule;
use move_core_types::account_address::AccountAddress;
use move_core_types::language_storage::{ModuleId, StructTag};
use move_core_types::parser::parse_struct_tag;
use move_core_types::resolver::{ModuleResolver, ResourceResolver};
use prometheus::Registry;
use similar::{ChangeTag, TextDiff};
use std::collections::{BTreeMap, HashSet};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::Mutex;
use sui_adapter::adapter;
use sui_adapter::execution_engine::execute_transaction_to_effects_impl;
use sui_adapter::execution_mode;
use sui_config::node::ExpensiveSafetyCheckConfig;
use sui_core::authority::authority_per_epoch_store::AuthorityPerEpochStore;
use sui_core::authority::epoch_start_configuration::EpochStartConfiguration;
use sui_core::authority::test_authority_builder::TestAuthorityBuilder;
use sui_core::authority::AuthorityState;
use sui_core::authority::TemporaryStore;
use sui_core::epoch::epoch_metrics::EpochMetrics;
use sui_core::module_cache_metrics::ResolverMetrics;
use sui_core::signature_verifier::SignatureVerifierMetrics;
use sui_framework::BuiltInFramework;
use sui_json_rpc_types::SuiTransactionBlockEffects;
use sui_json_rpc_types::SuiTransactionBlockEffectsAPI;
use sui_json_rpc_types::{EventFilter, SuiEvent};
use sui_protocol_config::ProtocolConfig;
use sui_sdk::{SuiClient, SuiClientBuilder};
use sui_types::base_types::{ObjectID, ObjectRef, SequenceNumber, SuiAddress, VersionNumber};
use sui_types::committee::EpochId;
use sui_types::digests::CheckpointDigest;
use sui_types::digests::TransactionDigest;
use sui_types::error::ExecutionError;
use sui_types::error::{SuiError, SuiResult};
use sui_types::executable_transaction::VerifiedExecutableTransaction;
use sui_types::gas::SuiGasStatus;
use sui_types::metrics::LimitsMetrics;
use sui_types::object::{Data, Object, Owner};
use sui_types::storage::get_module_by_id;
use sui_types::storage::{BackingPackageStore, ChildObjectResolver, ObjectStore, ParentSync};
use sui_types::sui_system_state::epoch_start_sui_system_state::EpochStartSystemState;
use sui_types::temporary_store::InnerTemporaryStore;
use sui_types::transaction::CertifiedTransaction;
use sui_types::transaction::Transaction;
use sui_types::transaction::TransactionData;
use sui_types::transaction::VerifiedTransaction;
use sui_types::transaction::{InputObjectKind, InputObjects, TransactionKind};
use sui_types::transaction::{SenderSignedData, TransactionDataAPI};
use sui_types::DEEPBOOK_PACKAGE_ID;
use tracing::{error, warn};

// TODO: add persistent cache. But perf is good enough already.
// TODO: handle safe mode

// The logic here is very testnet specific now
// For testnet, we derive the protocol version map with some nuances due to early safe mode speedrun
// For other networks it should be much more straightforward

#[derive(Debug)]
pub struct ExecutionSandboxState {
    /// Information describing the transaction
    pub transaction_info: OnChainTransactionInfo,
    /// All the obejcts that are required for the execution of the transaction
    pub required_objects: Vec<Object>,
    /// Temporary store from executing this locally in `execute_transaction_to_effects_impl`
    pub local_exec_temporary_store: Option<InnerTemporaryStore>,
    /// Effects from executing this locally in `execute_transaction_to_effects_impl`
    pub local_exec_effects: SuiTransactionBlockEffects,
    /// Status from executing this locally in `execute_transaction_to_effects_impl`
    pub local_exec_status: Result<(), ExecutionError>,
}

impl ExecutionSandboxState {
    pub fn check_effects(&self) -> Result<(), LocalExecError> {
        if self.transaction_info.effects != self.local_exec_effects {
            error!("Replay tool forked {}", self.transaction_info.tx_digest);
            return Err(LocalExecError::EffectsForked {
                digest: self.transaction_info.tx_digest,
                diff: format!("\n{}", self.diff_effects()),
                on_chain: Box::new(self.transaction_info.effects.clone()),
                local: Box::new(self.local_exec_effects.clone()),
            });
        }
        Ok(())
    }

    /// Utility to diff effects in a human readable format
    pub fn diff_effects(&self) -> String {
        let eff1 = &self.transaction_info.effects;
        let eff2 = &self.local_exec_effects;
        let on_chain_str = format!("{:#?}", eff1);
        let local_chain_str = format!("{:#?}", eff2);
        let mut res = vec![];

        let diff = TextDiff::from_lines(&on_chain_str, &local_chain_str);
        println!("On-chain vs local diff");
        for change in diff.iter_all_changes() {
            let sign = match change.tag() {
                ChangeTag::Delete => "---",
                ChangeTag::Insert => "+++",
                ChangeTag::Equal => "   ",
            };
            res.push(format!("{}{}", sign, change));
        }

        res.join("")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolVersionSummary {
    /// Protocol version at this point
    pub protocol_version: u64,
    /// The first epoch that uses this protocol version
    pub epoch_start: u64,
    /// The last epoch that uses this protocol version
    pub epoch_end: u64,
    /// The first checkpoint in this protocol v ersion
    pub checkpoint_start: u64,
    /// The last checkpoint in this protocol version
    pub checkpoint_end: u64,
    /// The transaction which triggered this epoch change
    pub epoch_change_tx: TransactionDigest,
}

pub struct Storage {
    /// These are objects at the frontier of the execution's view
    /// They might not be the latest object currently but they are the latest objects
    /// for the TX at the time it was run
    /// This store cannot be shared between runners
    pub live_objects_store: BTreeMap<ObjectID, Object>,

    /// Package cache and object version cache can be shared between runners
    /// Non system packages are immutable so we can cache these
    pub package_cache: Arc<Mutex<BTreeMap<ObjectID, Object>>>,
    /// Object contents are frozen at their versions so we can cache these
    /// We must place system packages here as well
    pub object_version_cache: Arc<Mutex<BTreeMap<(ObjectID, SequenceNumber), Object>>>,
}

impl std::fmt::Display for Storage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "Live object store")?;
        for (id, obj) in self.live_objects_store.iter() {
            writeln!(f, "{}: {:?}", id, obj.compute_object_reference())?;
        }
        writeln!(f, "Package cache")?;
        for (id, obj) in self.package_cache.lock().expect("Unable to lock").iter() {
            writeln!(f, "{}: {:?}", id, obj.compute_object_reference())?;
        }
        writeln!(f, "Object version cache")?;
        for (id, _) in self
            .object_version_cache
            .lock()
            .expect("Unable to lock")
            .iter()
        {
            writeln!(f, "{}: {}", id.0, id.1)?;
        }

        write!(f, "")
    }
}

impl Storage {
    pub fn default() -> Self {
        Self {
            live_objects_store: BTreeMap::new(),
            package_cache: Arc::new(Mutex::new(BTreeMap::new())),
            object_version_cache: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    pub fn all_objects(&self) -> Vec<Object> {
        self.live_objects_store
            .values()
            .cloned()
            .chain(
                self.package_cache
                    .lock()
                    .expect("Unable to lock")
                    .iter()
                    .map(|(_, obj)| obj.clone()),
            )
            .chain(
                self.object_version_cache
                    .lock()
                    .expect("Unable to lock")
                    .iter()
                    .map(|(_, obj)| obj.clone()),
            )
            .collect::<Vec<_>>()
    }
}

pub struct LocalExec {
    pub client: SuiClient,
    // For a given protocol version, what TX created it, and what is the valid range of epochs
    // at this protocol version.
    pub protocol_version_epoch_table: BTreeMap<u64, ProtocolVersionSummary>,
    // For a given protocol version, the mapping valid sequence numbers for each framework package
    pub protocol_version_system_package_table: BTreeMap<u64, BTreeMap<ObjectID, SequenceNumber>>,
    // The current protocol version for this execution
    pub current_protocol_version: u64,
    // All state is contained here
    pub storage: Storage,
    // Debug events
    pub exec_store_events: Arc<Mutex<Vec<ExecutionStoreEvent>>>,
    // Debug events
    pub metrics: Arc<LimitsMetrics>,
    // Used for fetching data from the network or remote store
    pub fetcher: RemoteFetcher,
    /// For special casing some logic
    pub is_testnet: bool,

    // Retry policies due to RPC errors
    pub num_retries_for_timeout: u32,
    pub sleep_period_for_timeout: std::time::Duration,
}

impl LocalExec {
    /// Wrapper around fetcher in case we want to add more functionality
    /// Such as fetching from local DB from snapshot
    pub async fn multi_download(
        &self,
        objs: &[(ObjectID, SequenceNumber)],
    ) -> Result<Vec<Object>, LocalExecError> {
        let mut num_retries_for_timeout = self.num_retries_for_timeout as i64;
        while num_retries_for_timeout >= 0 {
            match self.fetcher.multi_get_versioned(objs).await {
                Ok(objs) => return Ok(objs),
                Err(LocalExecError::SuiRpcRequestTimeout) => {
                    warn!(
                        "RPC request timed out. Retries left {}. Sleeping for {}s",
                        num_retries_for_timeout,
                        self.sleep_period_for_timeout.as_secs()
                    );
                    num_retries_for_timeout -= 1;
                    tokio::time::sleep(self.sleep_period_for_timeout).await;
                }
                Err(e) => return Err(e),
            }
        }
        Err(LocalExecError::SuiRpcRequestTimeout)
    }
    /// Wrapper around fetcher in case we want to add more functionality
    /// Such as fetching from local DB from snapshot
    pub async fn multi_download_latest(
        &self,
        objs: &[ObjectID],
    ) -> Result<Vec<Object>, LocalExecError> {
        let mut num_retries_for_timeout = self.num_retries_for_timeout as i64;
        while num_retries_for_timeout >= 0 {
            match self.fetcher.multi_get_latest(objs).await {
                Ok(objs) => return Ok(objs),
                Err(LocalExecError::SuiRpcRequestTimeout) => {
                    warn!(
                        "RPC request timed out. Retries left {}. Sleeping for {}s",
                        num_retries_for_timeout,
                        self.sleep_period_for_timeout.as_secs()
                    );
                    num_retries_for_timeout -= 1;
                    tokio::time::sleep(self.sleep_period_for_timeout).await;
                }
                Err(e) => return Err(e),
            }
        }
        Err(LocalExecError::SuiRpcRequestTimeout)
    }

    pub async fn fetch_loaded_child_refs(
        &self,
        tx_digest: &TransactionDigest,
    ) -> Result<Vec<(ObjectID, SequenceNumber)>, LocalExecError> {
        // Get the child objects loaded
        self.fetcher.get_loaded_child_objects(tx_digest).await
    }

    /// Gets all the epoch change events
    pub async fn get_epoch_change_events(
        &self,
        reverse: bool,
    ) -> Result<impl Iterator<Item = SuiEvent>, LocalExecError> {
        let struct_tag_str = EPOCH_CHANGE_STRUCT_TAG.to_string();
        let struct_tag = parse_struct_tag(&struct_tag_str)?;

        // TODO: Should probably limit/page this but okay for now?
        Ok(self
            .client
            .event_api()
            .query_events(EventFilter::MoveEventType(struct_tag), None, None, reverse)
            .await
            .map_err(|e| LocalExecError::UnableToQuerySystemEvents {
                rpc_err: e.to_string(),
            })?
            .data
            .into_iter())
    }

    pub async fn new_from_fn_url(http_url: &str) -> Result<Self, LocalExecError> {
        Self::new(
            SuiClientBuilder::default()
                .request_timeout(RPC_TIMEOUT_ERR_SLEEP_RETRY_PERIOD)
                .max_concurrent_requests(MAX_CONCURRENT_REQUESTS)
                .build(http_url)
                .await?,
        )
        .await
    }

    /// This captures the state of the network at a given point in time and populates
    /// prptocol version tables including which system packages to fetch
    /// If this function is called across epoch boundaries, the info might be stale.
    /// But it should only be called once per epoch.
    pub async fn init_for_execution(mut self) -> Result<Self, LocalExecError> {
        self.populate_protocol_version_tables().await?;
        Ok(self)
    }

    pub async fn reset_for_new_execution(self) -> Result<Self, LocalExecError> {
        Self::new(self.client).await?.init_for_execution().await
    }

    pub async fn new(client: SuiClient) -> Result<Self, LocalExecError> {
        // Use a throwaway metrics registry for local execution.
        let registry = prometheus::Registry::new();
        let metrics = Arc::new(LimitsMetrics::new(&registry));

        let fetcher = RemoteFetcher {
            rpc_client: client.clone(),
        };

        let is_testnet = fetcher
            .get_checkpoint_txs(0)
            .await?
            .iter()
            .any(|tx| tx == &TransactionDigest::from_str(TESTNET_GENESIX_TX_DIGEST).unwrap());

        Ok(Self {
            client,
            protocol_version_epoch_table: BTreeMap::new(),
            protocol_version_system_package_table: BTreeMap::new(),
            current_protocol_version: 0,
            exec_store_events: Arc::new(Mutex::new(Vec::new())),
            metrics,
            storage: Storage::default(),
            fetcher,
            is_testnet,
            // TODO: make these configurable
            num_retries_for_timeout: RPC_TIMEOUT_ERR_NUM_RETRIES,
            sleep_period_for_timeout: RPC_TIMEOUT_ERR_SLEEP_RETRY_PERIOD,
        })
    }

    #[allow(clippy::wrong_self_convention)]
    pub fn to_temporary_store(
        &mut self,
        tx_digest: &TransactionDigest,
        input_objects: InputObjects,
        protocol_config: &ProtocolConfig,
    ) -> TemporaryStore<&mut LocalExec> {
        TemporaryStore::new(self, input_objects, *tx_digest, protocol_config)
    }

    pub async fn multi_download_and_store(
        &mut self,
        objs: &[(ObjectID, SequenceNumber)],
    ) -> Result<Vec<Object>, LocalExecError> {
        let objs = self.multi_download(objs).await?;

        // Backfill the store
        for obj in objs.iter() {
            let o_ref = obj.compute_object_reference();
            self.storage.live_objects_store.insert(o_ref.0, obj.clone());
            self.storage
                .object_version_cache
                .lock()
                .expect("Cannot lock")
                .insert((o_ref.0, o_ref.1), obj.clone());
            if obj.is_package() {
                self.storage
                    .package_cache
                    .lock()
                    .expect("Cannot lock")
                    .insert(o_ref.0, obj.clone());
            }
        }
        Ok(objs)
    }

    pub async fn multi_download_relevant_packages_and_store(
        &mut self,
        objs: Vec<ObjectID>,
        protocol_version: u64,
    ) -> Result<Vec<Object>, LocalExecError> {
        let syst_packages = self.system_package_versions_for_epoch(protocol_version)?;
        let syst_packages_objs = self.multi_download(&syst_packages).await?;

        // Download latest version of all packages that are not system packages
        // This is okay since the versions can never change
        let non_system_package_objs: Vec<_> = objs
            .into_iter()
            .filter(|o| !Self::system_package_ids(self.current_protocol_version).contains(o))
            .collect();
        let objs = self
            .multi_download_latest(&non_system_package_objs)
            .await?
            .into_iter()
            .chain(syst_packages_objs.into_iter());

        for obj in objs.clone() {
            let o_ref = obj.compute_object_reference();
            // We dont always want the latest in store
            //self.storage.store.insert(o_ref.0, obj.clone());
            self.storage
                .object_version_cache
                .lock()
                .expect("Cannot lock")
                .insert((o_ref.0, o_ref.1), obj.clone());
            if obj.is_package() {
                self.storage
                    .package_cache
                    .lock()
                    .expect("Cannot lock")
                    .insert(o_ref.0, obj.clone());
            }
        }
        Ok(objs.collect())
    }

    // TODO: remove this after `futures::executor::block_on` is removed.
    #[allow(clippy::disallowed_methods)]
    pub fn download_object(
        &self,
        object_id: &ObjectID,
        version: SequenceNumber,
    ) -> Result<Object, LocalExecError> {
        if self
            .storage
            .object_version_cache
            .lock()
            .expect("Cannot lock")
            .contains_key(&(*object_id, version))
        {
            return Ok(self
                .storage
                .object_version_cache
                .lock()
                .expect("Cannot lock")
                .get(&(*object_id, version))
                .ok_or(LocalExecError::InternalCacheInvariantViolation {
                    id: *object_id,
                    version: Some(version),
                })?
                .clone());
        }

        let o = block_on(self.multi_download(&[(*object_id, version)])).map(|mut q| {
            q.pop().unwrap_or_else(|| {
                panic!(
                    "Downloaded obj response cannot be empty {:?}",
                    (*object_id, version)
                )
            })
        })?;

        let o_ref = o.compute_object_reference();
        self.storage
            .object_version_cache
            .lock()
            .expect("Cannot lock")
            .insert((o_ref.0, o_ref.1), o.clone());
        Ok(o)
    }

    // TODO: remove this after `futures::executor::block_on` is removed.
    #[allow(clippy::disallowed_methods)]
    pub fn download_latest_object(
        &self,
        object_id: &ObjectID,
    ) -> Result<Option<Object>, LocalExecError> {
        let resp = block_on({
            //info!("Downloading latest object {object_id}");
            self.multi_download_latest(&[*object_id])
        })
        .map(|mut q| {
            q.pop()
                .unwrap_or_else(|| panic!("Downloaded obj response cannot be empty {}", *object_id))
        });

        match resp {
            Ok(v) => Ok(Some(v)),
            Err(LocalExecError::ObjectNotExist { id }) => {
                error!("Could not find object {id} on RPC server. It might have been pruned, deleted, or never existed.");
                Ok(None)
            }
            Err(LocalExecError::ObjectDeleted {
                id,
                version,
                digest,
            }) => {
                error!("Object {id} {version} {digest} was deleted on RPC server.");
                Ok(None)
            }
            Err(err) => Err(LocalExecError::SuiRpcError {
                err: err.to_string(),
            }),
        }
    }

    pub async fn get_checkpoint_txs(
        &self,
        checkpoint_id: u64,
    ) -> Result<Vec<TransactionDigest>, LocalExecError> {
        self.fetcher
            .get_checkpoint_txs(checkpoint_id)
            .await
            .map_err(|e| LocalExecError::SuiRpcError { err: e.to_string() })
    }

    pub async fn execute_all_in_checkpoints(
        &mut self,
        checkpoint_ids: &[u64],
        expensive_safety_check_config: &ExpensiveSafetyCheckConfig,
        terminate_early: bool,
        use_authority: bool,
    ) -> Result<(u64, u64), LocalExecError> {
        // Get all the TXs at this checkpoint
        let mut txs = Vec::new();
        for checkpoint_id in checkpoint_ids {
            txs.extend(self.get_checkpoint_txs(*checkpoint_id).await?);
        }
        let num = txs.len();
        let mut succeeded = 0;
        for tx in txs {
            match self
                .execute_transaction(&tx, expensive_safety_check_config.clone(), use_authority)
                .await
                .map(|q| q.check_effects())
            {
                Err(e) | Ok(Err(e)) => {
                    if terminate_early {
                        return Err(e);
                    }
                    error!("Error executing tx: {},  {:#?}", tx, e);
                    continue;
                }
                _ => (),
            }

            succeeded += 1;
        }
        Ok((succeeded, num as u64))
    }

    pub async fn execution_engine_execute_with_tx_info_impl(
        &mut self,
        tx_info: &OnChainTransactionInfo,
        override_transaction_kind: Option<TransactionKind>,
        expensive_safety_check_config: ExpensiveSafetyCheckConfig,
    ) -> Result<ExecutionSandboxState, LocalExecError> {
        let tx_digest = &tx_info.tx_digest;
        // A lot of the logic here isnt designed for genesis
        if *tx_digest == TransactionDigest::genesis() || tx_info.sender == SuiAddress::ZERO {
            // Genesis.
            warn!(
                "Genesis replay not supported: {}, skipping transaction",
                tx_digest
            );
            // Return the same data from onchain since we dont want to fail nor do we want to recompute
            // Assume genesis transactions are always successful
            let effects = tx_info.effects.clone();
            return Ok(ExecutionSandboxState {
                transaction_info: tx_info.clone(),
                required_objects: vec![],
                local_exec_temporary_store: None,
                local_exec_effects: effects,
                local_exec_status: Ok(()),
            });
        }

        // Initialize the state necessary for execution
        // Get the input objects
        let input_objects = self.initialize_execution_env_state(tx_info).await?;

        // At this point we have all the objects needed for replay

        // This assumes we already initialized the protocol version table `protocol_version_epoch_table`
        let protocol_config = &tx_info.protocol_config;

        let metrics = self.metrics.clone();

        // Extract the epoch start timestamp
        let (epoch_start_timestamp, _) = self
            .get_epoch_start_timestamp_and_rgp(tx_info.executed_epoch)
            .await?;

        // Create the gas status
        let gas_status =
            SuiGasStatus::new_with_budget(tx_info.gas_budget, tx_info.gas_price, protocol_config);

        // Temp store for data
        let temporary_store =
            self.to_temporary_store(tx_digest, InputObjects::new(input_objects), protocol_config);

        // We could probably cache the VM per protocol config
        let move_vm = get_vm(protocol_config, expensive_safety_check_config)?;

        // All prep done
        let res = execute_transaction_to_effects_impl::<execution_mode::Normal, _>(
            tx_info.shared_object_refs.clone(),
            temporary_store,
            override_transaction_kind.unwrap_or(tx_info.kind.clone()),
            tx_info.sender,
            &tx_info.gas.clone(),
            *tx_digest,
            tx_info.dependencies.clone().into_iter().collect(),
            &move_vm,
            gas_status,
            &tx_info.executed_epoch,
            epoch_start_timestamp,
            protocol_config,
            metrics,
            true,
            &HashSet::new(),
        );

        let all_required_objects = self.storage.all_objects();
        let effects = SuiTransactionBlockEffects::try_from(res.1).map_err(LocalExecError::from)?;

        Ok(ExecutionSandboxState {
            transaction_info: tx_info.clone(),
            required_objects: all_required_objects,
            local_exec_temporary_store: Some(res.0),
            local_exec_effects: effects,
            local_exec_status: res.2,
        })
    }

    /// Must be called after `init_for_execution`
    pub async fn execution_engine_execute_impl(
        &mut self,
        tx_digest: &TransactionDigest,
        expensive_safety_check_config: ExpensiveSafetyCheckConfig,
    ) -> Result<ExecutionSandboxState, LocalExecError> {
        assert!(
            !self.protocol_version_system_package_table.is_empty()
                || !self.protocol_version_epoch_table.is_empty(),
            "Required tables not populated. Must call `init_for_execution` before executing transactions"
        );

        let tx_info = self.resolve_tx_components(tx_digest).await?;
        self.execution_engine_execute_with_tx_info_impl(
            &tx_info,
            None,
            expensive_safety_check_config,
        )
        .await
    }

    /// Executes a transaction with the state specified in `pre_run_sandbox`
    /// This is useful for executing a transaction with a specific state
    /// However if the state in invalid, the behavior is undefined. Use wisely
    /// If no transaction is provided, the transaction in the sandbox state is used
    /// Currently if the transaction is provided, the signing will fail, so this feature is TBD
    pub async fn certificate_execute_with_sandbox_state(
        &mut self,
        pre_run_sandbox: &ExecutionSandboxState,
        override_transaction_data: Option<TransactionData>,
    ) -> Result<ExecutionSandboxState, LocalExecError> {
        assert!(
            override_transaction_data.is_none(),
            "Custom transaction data is not supported yet"
        );

        // These cannot be changed and are inherited from the sandbox state
        let executed_epoch = pre_run_sandbox.transaction_info.executed_epoch;
        let reference_gas_price = pre_run_sandbox.transaction_info.reference_gas_price;
        let epoch_start_timestamp = pre_run_sandbox.transaction_info.epoch_start_timestamp;
        let protocol_config = pre_run_sandbox.transaction_info.protocol_config.clone();
        let required_objects = pre_run_sandbox.required_objects.clone();
        let shared_object_refs = pre_run_sandbox.transaction_info.shared_object_refs.clone();

        let transaction_intent = pre_run_sandbox
            .transaction_info
            .sender_signed_data
            .intent_message()
            .intent
            .clone();
        let transaction_signatures = pre_run_sandbox
            .transaction_info
            .sender_signed_data
            .tx_signatures()
            .to_vec();

        // This must be provided
        let transaction_data = override_transaction_data.unwrap_or(
            pre_run_sandbox
                .transaction_info
                .sender_signed_data
                .transaction_data()
                .clone(),
        );

        // Begin state prep
        let (authority_state, epoch_store) = prep_network(
            &required_objects,
            reference_gas_price,
            executed_epoch,
            epoch_start_timestamp,
            &protocol_config,
        )
        .await;

        let sender_signed_tx = Transaction::from_generic_sig_data(
            transaction_data,
            transaction_intent,
            transaction_signatures,
        );
        let sender_signed_tx = VerifiedTransaction::new_unchecked(
            VerifiedTransaction::new_unchecked(sender_signed_tx).into(),
        );

        let response = authority_state
            .handle_transaction(&epoch_store, sender_signed_tx.clone())
            .await?;

        let auth_vote = response.status.into_signed_for_testing();

        let mut committee = authority_state.clone_committee_for_testing();
        committee.epoch = executed_epoch;
        let certificate = CertifiedTransaction::new(
            sender_signed_tx.into_message(),
            vec![auth_vote.clone()],
            &committee,
        )
        .unwrap()
        .verify(&committee)
        .unwrap();

        let certificate = &VerifiedExecutableTransaction::new_from_certificate(certificate.clone());

        let new_tx_digest = certificate.digest();

        epoch_store
            .set_shared_object_versions_for_testing(
                new_tx_digest,
                &shared_object_refs
                    .iter()
                    .map(|(id, version, _)| (*id, *version))
                    .collect::<Vec<_>>(),
            )
            .unwrap();

        // hack to simulate an epoch change just for this transaction
        {
            let db = authority_state.db();
            let mut execution_lock = db.execution_lock_for_reconfiguration().await;
            *execution_lock = executed_epoch;
            drop(execution_lock);
        }

        let res = authority_state
            .try_execute_immediately(certificate, None, &epoch_store)
            .await
            .unwrap();

        let exec_res = match res.1 {
            Some(q) => Err(q),
            None => Ok(()),
        };
        let effects = SuiTransactionBlockEffects::try_from(res.0).map_err(LocalExecError::from)?;

        Ok(ExecutionSandboxState {
            transaction_info: pre_run_sandbox.transaction_info.clone(),
            required_objects,
            local_exec_temporary_store: None, // We dont capture it for cert exec run
            local_exec_effects: effects,
            local_exec_status: exec_res,
        })
    }

    /// Must be called after `init_for_execution`
    /// This executes from `sui_core::authority::AuthorityState::try_execute_immediately`
    pub async fn certificate_execute(
        &mut self,
        tx_digest: &TransactionDigest,
        expensive_safety_check_config: ExpensiveSafetyCheckConfig,
    ) -> Result<ExecutionSandboxState, LocalExecError> {
        // Use the lighterweight execution engine to get the pre-run state
        let pre_run_sandbox = self
            .execution_engine_execute_impl(tx_digest, expensive_safety_check_config)
            .await?;
        self.certificate_execute_with_sandbox_state(&pre_run_sandbox, None)
            .await
    }

    /// Must be called after `init_for_execution`
    /// This executes from `sui_adapter::execution_engine::execute_transaction_to_effects_impl`
    pub async fn execution_engine_execute(
        &mut self,
        tx_digest: &TransactionDigest,
        expensive_safety_check_config: ExpensiveSafetyCheckConfig,
    ) -> Result<ExecutionSandboxState, LocalExecError> {
        let sandbox_state = self
            .execution_engine_execute_impl(tx_digest, expensive_safety_check_config)
            .await?;

        Ok(sandbox_state)
    }

    pub async fn execute_transaction(
        &mut self,
        tx_digest: &TransactionDigest,
        expensive_safety_check_config: ExpensiveSafetyCheckConfig,
        use_authority: bool,
    ) -> Result<ExecutionSandboxState, LocalExecError> {
        if use_authority {
            self.certificate_execute(tx_digest, expensive_safety_check_config.clone())
                .await
        } else {
            self.execution_engine_execute(tx_digest, expensive_safety_check_config)
                .await
        }
    }
    fn system_package_ids(protocol_version: u64) -> Vec<ObjectID> {
        let mut ids = BuiltInFramework::all_package_ids();

        if protocol_version < 5 {
            ids.retain(|id| *id != DEEPBOOK_PACKAGE_ID)
        }
        ids
    }

    /// This is the only function which accesses the network during execution
    pub fn get_or_download_object(
        &self,
        obj_id: &ObjectID,
        package_expected: bool,
    ) -> Result<Option<Object>, LocalExecError> {
        if package_expected {
            if let Some(obj) = self
                .storage
                .package_cache
                .lock()
                .expect("Cannot lock")
                .get(obj_id)
            {
                return Ok(Some(obj.clone()));
            };
            // Check if its a system package because we must've downloaded all
            // TODO: Will return this check once we can download completely for other networks
            // assert!(
            //     !self.system_package_ids().contains(obj_id),
            //     "All system packages should be downloaded already"
            // );
        } else if let Some(obj) = self.storage.live_objects_store.get(obj_id) {
            return Ok(Some(obj.clone()));
        }

        let Some(o) =  self.download_latest_object(obj_id)? else { return Ok(None) };

        if o.is_package() {
            assert!(
                package_expected,
                "Did not expect package but downloaded object is a package: {obj_id}"
            );

            self.storage
                .package_cache
                .lock()
                .expect("Cannot lock")
                .insert(*obj_id, o.clone());
        }
        let o_ref = o.compute_object_reference();
        self.storage
            .object_version_cache
            .lock()
            .expect("Cannot lock")
            .insert((o_ref.0, o_ref.1), o.clone());
        Ok(Some(o))
    }

    /// Must be called after `populate_protocol_version_tables`
    pub fn system_package_versions_for_epoch(
        &self,
        epoch: u64,
    ) -> Result<Vec<(ObjectID, SequenceNumber)>, LocalExecError> {
        Ok(self
            .protocol_version_system_package_table
            .get(&epoch)
            .ok_or(LocalExecError::FrameworkObjectVersionTableNotPopulated { epoch })?
            .clone()
            .into_iter()
            .collect())
    }

    /// Very testnet specific now
    /// This function is testnet specific and will be extended for other networs later
    pub async fn protocol_ver_to_epoch_map(
        &self,
    ) -> Result<BTreeMap<u64, ProtocolVersionSummary>, LocalExecError> {
        let mut range_map = BTreeMap::new();
        let epoch_change_events = self.get_epoch_change_events(false).await?;

        // Exception for Genesis: Protocol version 1 at epoch 0
        let mut tx_digest = TransactionDigest::from_str(TESTNET_GENESIX_TX_DIGEST).unwrap();
        // Somehow the genesis TX did not emit any event, but we know it was the start of version 1
        // So we need to manually add this range
        let (mut start_epoch, mut start_protocol_version, mut start_checkpoint) = (0, 1, 0u64);

        let (mut curr_epoch, mut curr_protocol_version, mut curr_checkpoint) =
            (start_epoch, start_protocol_version, start_checkpoint);

        if self.is_testnet {
            // Exception for incident: Protocol version 2 started epoch 742
            // But this was in safe mode so no events emitted
            // So we need to manually add this range
            (curr_epoch, curr_protocol_version) = (742, 2);
            curr_checkpoint = self
                .fetcher
                .get_transaction(&TransactionDigest::from_str(SAFE_MODE_TX_1_DIGEST).unwrap())
                .await?
                .checkpoint
                .expect("Checkpoint should be present");
            range_map.insert(
                start_protocol_version,
                ProtocolVersionSummary {
                    protocol_version: start_protocol_version,
                    epoch_start: start_epoch,
                    epoch_end: curr_epoch - 1,
                    checkpoint_start: start_checkpoint,
                    checkpoint_end: curr_checkpoint - 1,
                    epoch_change_tx: tx_digest,
                },
            );
        }

        (start_epoch, start_protocol_version, start_checkpoint) =
            (curr_epoch, curr_protocol_version, curr_checkpoint);
        tx_digest = TransactionDigest::from_str(SAFE_MODE_TX_1_DIGEST).unwrap();

        // This is the final tx digest for the epoch change. We need this to track the final checkpoint
        let mut end_epoch_tx_digest = tx_digest;

        for event in epoch_change_events {
            (curr_epoch, curr_protocol_version) = extract_epoch_and_version(event.clone())?;
            end_epoch_tx_digest = event.id.tx_digest;

            if self.is_testnet && (curr_protocol_version < 3) {
                // Ignore protocol versions before 3 as we've handled before the loop
                continue;
            }

            if start_protocol_version == curr_protocol_version {
                // Same range
                continue;
            }

            // Change in prot version
            // Find the last checkpoint
            curr_checkpoint = self
                .fetcher
                .get_transaction(&event.id.tx_digest)
                .await?
                .checkpoint
                .expect("Checkpoint should be present");
            // Insert the last range
            range_map.insert(
                start_protocol_version,
                ProtocolVersionSummary {
                    protocol_version: start_protocol_version,
                    epoch_start: start_epoch,
                    epoch_end: curr_epoch - 1,
                    checkpoint_start: start_checkpoint,
                    checkpoint_end: curr_checkpoint - 1,
                    epoch_change_tx: tx_digest,
                },
            );

            start_epoch = curr_epoch;
            start_protocol_version = curr_protocol_version;
            tx_digest = event.id.tx_digest;
            start_checkpoint = curr_checkpoint;
        }

        // Insert the last range
        range_map.insert(
            curr_protocol_version,
            ProtocolVersionSummary {
                protocol_version: curr_protocol_version,
                epoch_start: start_epoch,
                epoch_end: curr_epoch,
                checkpoint_start: curr_checkpoint,
                checkpoint_end: self
                    .fetcher
                    .get_transaction(&end_epoch_tx_digest)
                    .await?
                    .checkpoint
                    .expect("Checkpoint should be present"),
                epoch_change_tx: tx_digest,
            },
        );

        Ok(range_map)
    }

    pub fn protocol_version_for_epoch(
        epoch: u64,
        mp: &BTreeMap<u64, (TransactionDigest, u64, u64)>,
    ) -> u64 {
        // Naive impl but works for now
        // Can improve with range algos & data structures
        let mut version = 1;
        for (k, v) in mp.iter().rev() {
            if v.1 <= epoch {
                version = *k;
                break;
            }
        }
        version
    }

    pub async fn populate_protocol_version_tables(&mut self) -> Result<(), LocalExecError> {
        self.protocol_version_epoch_table = self.protocol_ver_to_epoch_map().await?;

        let system_package_revisions = self.system_package_versions().await?;

        // This can be more efficient but small footprint so okay for now
        //Table is sorted from earliest to latest
        for (
            prot_ver,
            ProtocolVersionSummary {
                epoch_change_tx: tx_digest,
                ..
            },
        ) in self.protocol_version_epoch_table.clone()
        {
            // Use the previous versions protocol version table
            let mut working = self
                .protocol_version_system_package_table
                .get_mut(&(prot_ver - 1))
                .unwrap_or(&mut BTreeMap::new())
                .clone();

            for (id, versions) in system_package_revisions.iter() {
                // Oldest appears first in list, so reverse
                for ver in versions.iter().rev() {
                    if ver.1 == tx_digest {
                        // Found the version for this protocol version
                        working.insert(*id, ver.0);
                        break;
                    }
                }
            }
            self.protocol_version_system_package_table
                .insert(prot_ver, working);
        }
        Ok(())
    }

    pub async fn system_package_versions(
        &self,
    ) -> Result<BTreeMap<ObjectID, Vec<(SequenceNumber, TransactionDigest)>>, LocalExecError> {
        let system_package_ids = Self::system_package_ids(
            *self
                .protocol_version_epoch_table
                .keys()
                .peekable()
                .last()
                .expect("Protocol version epoch table not populated"),
        );
        let mut system_package_objs = self.multi_download_latest(&system_package_ids).await?;

        let mut mapping = BTreeMap::new();

        // Extract all the transactions which created or mutated this object
        while !system_package_objs.is_empty() {
            // For the given object and its version, record the transaction which upgraded or created it
            let previous_txs: Vec<_> = system_package_objs
                .iter()
                .map(|o| (o.compute_object_reference(), o.previous_transaction))
                .collect();

            previous_txs.iter().for_each(|((id, ver, _), tx)| {
                mapping.entry(*id).or_insert(vec![]).push((*ver, *tx));
            });

            // Next round
            // Get the previous version of each object if exists
            let previous_ver_refs: Vec<_> = previous_txs
                .iter()
                .filter_map(|(q, _)| {
                    let prev_ver = u64::from(q.1) - 1;
                    if prev_ver == 0 {
                        None
                    } else {
                        Some((q.0, SequenceNumber::from(prev_ver)))
                    }
                })
                .collect();
            system_package_objs = match self.multi_download(&previous_ver_refs).await {
                Ok(packages) => packages,
                Err(LocalExecError::ObjectNotExist { id }) => {
                    // This happens when the RPC server prunes older object
                    // Replays in the current protocol version will work but old ones might not
                    // as we cannot fetch the package
                    warn!("Object {} does not exist on RPC server. This might be due to pruning. Historical replays might not work", id);
                    break;
                }
                Err(LocalExecError::ObjectVersionNotFound { id, version }) => {
                    // This happens when the RPC server prunes older object
                    // Replays in the current protocol version will work but old ones might not
                    // as we cannot fetch the package
                    warn!("Object {} at version {} does not exist on RPC server. This might be due to pruning. Historical replays might not work", id, version);
                    break;
                }
                Err(LocalExecError::ObjectVersionTooHigh {
                    id,
                    asked_version,
                    latest_version,
                }) => {
                    warn!("Object {} at version {} does not exist on RPC server. Latest version is {}. This might be due to pruning. Historical replays might not work", id, asked_version,latest_version );
                    break;
                }
                Err(LocalExecError::ObjectDeleted {
                    id,
                    version,
                    digest,
                }) => {
                    // This happens when the RPC server prunes older object
                    // Replays in the current protocol version will work but old ones might not
                    // as we cannot fetch the package
                    warn!("Object {} at version {} digest {} deleted from RPC server. This might be due to pruning. Historical replays might not work", id, version, digest);
                    break;
                }
                Err(e) => return Err(e),
            };
        }
        Ok(mapping)
    }

    pub async fn get_protocol_config(
        &self,
        epoch_id: EpochId,
    ) -> Result<ProtocolConfig, LocalExecError> {
        self.protocol_version_epoch_table
            .iter()
            .rev()
            .find(|(_, rg)| epoch_id >= rg.epoch_start)
            .map(|(p, _rg)| Ok(ProtocolConfig::get_for_version((*p).into())))
            .unwrap_or_else(|| Err(LocalExecError::ProtocolVersionNotFound { epoch: epoch_id }))
    }

    pub async fn checkpoints_for_epoch(&self, epoch_id: u64) -> Result<(u64, u64), LocalExecError> {
        let epoch_change_events = self
            .get_epoch_change_events(true)
            .await?
            .collect::<Vec<_>>();
        let (start_checkpoint, start_epoch_idx) = if epoch_id == 0 {
            (0, 1)
        } else {
            let idx = epoch_change_events
                .iter()
                .position(|ev| match extract_epoch_and_version(ev.clone()) {
                    Ok((epoch, _)) => epoch == epoch_id,
                    Err(_) => false,
                })
                .ok_or(LocalExecError::EventNotFound { epoch: epoch_id })?;
            let epoch_change_tx = epoch_change_events[idx].id.tx_digest;
            (
                self.fetcher
                    .get_transaction(&epoch_change_tx)
                    .await?
                    .checkpoint
                    .expect("Checkpoint should be present"),
                idx,
            )
        };

        let next_epoch_change_tx = epoch_change_events
            .get(start_epoch_idx + 1)
            .map(|v| v.id.tx_digest)
            .ok_or(LocalExecError::UnableToDetermineCheckpoint { epoch: epoch_id })?;

        let next_epoch_checkpoint = self
            .fetcher
            .get_transaction(&next_epoch_change_tx)
            .await?
            .checkpoint
            .expect("Checkpoint should be present");

        Ok((start_checkpoint, next_epoch_checkpoint - 1))
    }

    /// Very testnet specific
    /// This function is testnet specific and will be extended for mainnet later
    pub async fn get_epoch_start_timestamp_and_rgp(
        &self,
        epoch_id: u64,
    ) -> Result<(u64, u64), LocalExecError> {
        // Hack for testnet: for epoch in range [3, 742), we have no data, but no user TX was executed, so return dummy
        if (self.is_testnet) && (2 < epoch_id) && (epoch_id < 742) {
            return Ok((0, 1));
        }

        let event = self
            .get_epoch_change_events(true)
            .await?
            .find(|ev| match extract_epoch_and_version(ev.clone()) {
                Ok((epoch, _)) => epoch == epoch_id,
                Err(_) => false,
            })
            .ok_or(LocalExecError::EventNotFound { epoch: epoch_id })?;

        let reference_gas_price = if let serde_json::Value::Object(w) = event.parsed_json {
            u64::from_str(&w["reference_gas_price"].to_string().replace('\"', "")).unwrap()
        } else {
            return Err(LocalExecError::UnexpectedEventFormat { event });
        };

        let epoch_change_tx = event.id.tx_digest;

        // Fetch full transaction content
        let tx_info = self.fetcher.get_transaction(&epoch_change_tx).await?;

        let orig_tx: SenderSignedData = bcs::from_bytes(&tx_info.raw_transaction).unwrap();
        let tx_kind_orig = orig_tx.transaction_data().kind();

        if let TransactionKind::ChangeEpoch(change) = tx_kind_orig {
            return Ok((change.epoch_start_timestamp_ms, reference_gas_price));
        }
        Err(LocalExecError::InvalidEpochChangeTx { epoch: epoch_id })
    }

    async fn resolve_tx_components(
        &self,
        tx_digest: &TransactionDigest,
    ) -> Result<OnChainTransactionInfo, LocalExecError> {
        // Fetch full transaction content
        let tx_info = self.fetcher.get_transaction(tx_digest).await?;
        let sender = match tx_info.clone().transaction.unwrap().data {
            sui_json_rpc_types::SuiTransactionBlockData::V1(tx) => tx.sender,
        };
        let SuiTransactionBlockEffects::V1(effects) = tx_info.clone().effects.unwrap();

        let raw_tx_bytes = tx_info.clone().raw_transaction;
        let orig_tx: SenderSignedData = bcs::from_bytes(&raw_tx_bytes).unwrap();
        let input_objs = orig_tx
            .transaction_data()
            .input_objects()
            .map_err(|e| LocalExecError::UserInputError { err: e })?;
        let tx_kind_orig = orig_tx.transaction_data().kind();

        // Download the objects at the version right before the execution of this TX
        let modified_at_versions: Vec<(ObjectID, SequenceNumber)> = effects.modified_at_versions();

        let shared_obj_refs = effects.shared_objects();
        let gas_data = match tx_info.clone().transaction.unwrap().data {
            sui_json_rpc_types::SuiTransactionBlockData::V1(tx) => tx.gas_data,
        };
        let gas_object_refs: Vec<_> = gas_data
            .payment
            .iter()
            .map(|obj_ref| obj_ref.to_object_ref())
            .collect();

        let epoch_id = effects.executed_epoch;

        // Extract the epoch start timestamp
        let (epoch_start_timestamp, reference_gas_price) =
            self.get_epoch_start_timestamp_and_rgp(epoch_id).await?;

        Ok(OnChainTransactionInfo {
            kind: tx_kind_orig.clone(),
            sender,
            modified_at_versions,
            input_objects: input_objs,
            shared_object_refs: shared_obj_refs.iter().map(|r| r.to_object_ref()).collect(),
            gas: gas_object_refs,
            gas_budget: gas_data.budget,
            gas_price: gas_data.price,
            executed_epoch: epoch_id,
            dependencies: effects.dependencies().to_vec(),
            effects: SuiTransactionBlockEffects::V1(effects),
            // Find the protocol version for this epoch
            // This assumes we already initialized the protocol version table `protocol_version_epoch_table`
            protocol_config: self.get_protocol_config(epoch_id).await?,
            tx_digest: *tx_digest,
            epoch_start_timestamp,
            sender_signed_data: orig_tx.clone(),
            reference_gas_price,
        })
    }

    async fn resolve_download_input_objects(
        &mut self,
        tx_info: &OnChainTransactionInfo,
    ) -> Result<Vec<(InputObjectKind, Object)>, LocalExecError> {
        // Download the input objects
        let mut package_inputs = vec![];
        let mut imm_owned_inputs = vec![];
        let mut shared_inputs = vec![];

        tx_info
            .input_objects
            .iter()
            .map(|kind| match kind {
                InputObjectKind::MovePackage(i) => {
                    package_inputs.push(*i);
                    Ok(())
                }
                InputObjectKind::ImmOrOwnedMoveObject(o_ref) => {
                    imm_owned_inputs.push((o_ref.0, o_ref.1));
                    Ok(())
                }
                InputObjectKind::SharedMoveObject {
                    id,
                    initial_shared_version: _,
                    mutable: _,
                } => {
                    // We already downloaded
                    if let Some(o) = self.storage.live_objects_store.get(id) {
                        shared_inputs.push(o.clone());
                        Ok(())
                    } else {
                        Err(LocalExecError::InternalCacheInvariantViolation {
                            id: *id,
                            version: None,
                        })
                    }
                }
            })
            .collect::<Result<Vec<_>, _>>()?;

        // Download the imm and owned objects
        let mut in_objs = self.multi_download_and_store(&imm_owned_inputs).await?;

        // For packages, download latest if non framework
        // If framework, download relevant for the current protocol version
        in_objs.extend(
            self.multi_download_relevant_packages_and_store(
                package_inputs,
                tx_info.protocol_config.version.as_u64(),
            )
            .await?,
        );
        // Add shared objects
        in_objs.extend(shared_inputs);

        let resolved_input_objs = tx_info
            .input_objects
            .iter()
            .map(|kind| match kind {
                InputObjectKind::MovePackage(i) => {
                    // Okay to unwrap since we downloaded it
                    (
                        *kind,
                        self.storage
                            .package_cache
                            .lock()
                            .expect("Cannot lock")
                            .get(i)
                            .unwrap_or(
                                &self
                                    .download_latest_object(i)
                                    .expect("Object download failed")
                                    .expect("Object not found on chain"),
                            )
                            .clone(),
                    )
                }
                InputObjectKind::ImmOrOwnedMoveObject(o_ref) => (
                    *kind,
                    self.storage
                        .object_version_cache
                        .lock()
                        .expect("Cannot lock")
                        .get(&(o_ref.0, o_ref.1))
                        .unwrap()
                        .clone(),
                ),
                InputObjectKind::SharedMoveObject {
                    id,
                    initial_shared_version: _,
                    mutable: _,
                } => {
                    // we already downloaded
                    (
                        *kind,
                        self.storage.live_objects_store.get(id).unwrap().clone(),
                    )
                }
            })
            .collect();

        Ok(resolved_input_objs)
    }

    /// Given the TxInfo, download and store the input objects, and other info necessary
    /// for execution
    async fn initialize_execution_env_state(
        &mut self,
        tx_info: &OnChainTransactionInfo,
    ) -> Result<Vec<(InputObjectKind, Object)>, LocalExecError> {
        // We need this for other activities in this session
        self.current_protocol_version = tx_info.protocol_config.version.as_u64();

        // Download the objects at the version right before the execution of this TX
        self.multi_download_and_store(&tx_info.modified_at_versions)
            .await?;

        // Download shared objects at the version right before the execution of this TX
        let shared_refs: Vec<_> = tx_info
            .shared_object_refs
            .iter()
            .map(|r| (r.0, r.1))
            .collect();
        self.multi_download_and_store(&shared_refs).await?;

        // Download gas (although this should already be in cache from modified at versions?)
        let gas_refs: Vec<_> = tx_info.gas.iter().map(|w| (w.0, w.1)).collect();
        self.multi_download_and_store(&gas_refs).await?;

        // Fetch the input objects we know from the raw transaction
        let input_objs = self.resolve_download_input_objects(tx_info).await?;

        // Prep the object runtime for dynamic fields
        // Download the child objects accessed at the version right before the execution of this TX
        let loaded_child_refs = self.fetch_loaded_child_refs(&tx_info.tx_digest).await?;
        self.multi_download_and_store(&loaded_child_refs).await?;

        Ok(input_objs)
    }
}

// <---------------------  Implement necessary traits for LocalExec to work with exec engine ----------------------->

impl BackingPackageStore for LocalExec {
    /// In this case we might need to download a dependency package which was not present in the
    /// modified at versions list because packages are immutable
    fn get_package_object(&self, package_id: &ObjectID) -> SuiResult<Option<Object>> {
        fn inner(self_: &LocalExec, package_id: &ObjectID) -> SuiResult<Option<Object>> {
            // If package not present fetch it from the network
            self_
                .get_or_download_object(package_id, true /* we expect a Move package*/)
                .map_err(|e| SuiError::GenericStorageError(e.to_string()))
        }

        let res = inner(self, package_id);
        self.exec_store_events
            .lock()
            .expect("Unable to lock events list")
            .push(ExecutionStoreEvent::BackingPackageGetPackageObject {
                package_id: *package_id,
                result: res.clone(),
            });
        res
    }
}

impl ChildObjectResolver for LocalExec {
    /// This uses `get_object`, which does not download from the network
    /// Hence all objects must be in store already
    fn read_child_object(&self, parent: &ObjectID, child: &ObjectID) -> SuiResult<Option<Object>> {
        fn inner(
            self_: &LocalExec,
            parent: &ObjectID,
            child: &ObjectID,
        ) -> SuiResult<Option<Object>> {
            let child_object = match self_.get_object(child)? {
                None => return Ok(None),
                Some(o) => o,
            };
            let parent = *parent;
            if child_object.owner != Owner::ObjectOwner(parent.into()) {
                return Err(SuiError::InvalidChildObjectAccess {
                    object: *child,
                    given_parent: parent,
                    actual_owner: child_object.owner,
                });
            }
            Ok(Some(child_object))
        }

        let res = inner(self, parent, child);
        self.exec_store_events
            .lock()
            .expect("Unable to lock events list")
            .push(
                ExecutionStoreEvent::ChildObjectResolverStoreReadChildObject {
                    parent: *parent,
                    child: *child,
                    result: res.clone(),
                },
            );
        res
    }
}

impl ParentSync for LocalExec {
    /// The objects here much already exist in the store because we downloaded them earlier
    /// No download from network
    fn get_latest_parent_entry_ref(&self, object_id: ObjectID) -> SuiResult<Option<ObjectRef>> {
        fn inner(self_: &LocalExec, object_id: ObjectID) -> SuiResult<Option<ObjectRef>> {
            if let Some(v) = self_.storage.live_objects_store.get(&object_id) {
                return Ok(Some(v.compute_object_reference()));
            }
            Ok(None)
        }
        let res = inner(self, object_id);
        self.exec_store_events
            .lock()
            .expect("Unable to lock events list")
            .push(
                ExecutionStoreEvent::ParentSyncStoreGetLatestParentEntryRef {
                    object_id,
                    result: res.clone(),
                },
            );
        res
    }
}

impl ResourceResolver for LocalExec {
    type Error = LocalExecError;

    /// In this case we might need to download a Move object on the fly which was not present in the
    /// modified at versions list because packages are immutable
    fn get_resource(
        &self,
        address: &AccountAddress,
        typ: &StructTag,
    ) -> Result<Option<Vec<u8>>, Self::Error> {
        fn inner(
            self_: &LocalExec,
            address: &AccountAddress,
            typ: &StructTag,
        ) -> Result<Option<Vec<u8>>, LocalExecError> {
            // If package not present fetch it from the network or some remote location
            let Some(object) = self_.get_or_download_object(
                &ObjectID::from(*address),false /* we expect a Move obj*/)? else {
                return Ok(None);
            };

            match &object.data {
                Data::Move(m) => {
                    assert!(
                        m.is_type(typ),
                        "Invariant violation: ill-typed object in storage \
                        or bad object request from caller"
                    );
                    Ok(Some(m.contents().to_vec()))
                }
                other => unimplemented!(
                    "Bad object lookup: expected Move object, but got {:#?}",
                    other
                ),
            }
        }

        let res = inner(self, address, typ);
        self.exec_store_events
            .lock()
            .expect("Unable to lock events list")
            .push(ExecutionStoreEvent::ResourceResolverGetResource {
                address: *address,
                typ: typ.clone(),
                result: res.clone(),
            });
        res
    }
}

impl ModuleResolver for LocalExec {
    type Error = LocalExecError;

    /// This fetches a module which must already be present in the store
    /// We do not download
    fn get_module(&self, module_id: &ModuleId) -> Result<Option<Vec<u8>>, Self::Error> {
        fn inner(
            self_: &LocalExec,
            module_id: &ModuleId,
        ) -> Result<Option<Vec<u8>>, LocalExecError> {
            Ok(self_
                .get_package(&ObjectID::from(*module_id.address()))
                .map_err(LocalExecError::from)?
                .and_then(|package| {
                    package
                        .serialized_module_map()
                        .get(module_id.name().as_str())
                        .cloned()
                }))
        }

        let res = inner(self, module_id);
        self.exec_store_events
            .lock()
            .expect("Unable to lock events list")
            .push(ExecutionStoreEvent::ModuleResolverGetModule {
                module_id: module_id.clone(),
                result: res.clone(),
            });
        res
    }
}

impl ModuleResolver for &mut LocalExec {
    type Error = LocalExecError;

    fn get_module(&self, module_id: &ModuleId) -> Result<Option<Vec<u8>>, Self::Error> {
        // Recording event here will be double-counting since its already recorded in the get_module fn
        (**self).get_module(module_id)
    }
}

impl ObjectStore for LocalExec {
    /// The object must be present in store by normal process we used to backfill store in init
    /// We dont download if not present
    fn get_object(&self, object_id: &ObjectID) -> Result<Option<Object>, SuiError> {
        let res = Ok(self.storage.live_objects_store.get(object_id).cloned());
        self.exec_store_events
            .lock()
            .expect("Unable to lock events list")
            .push(ExecutionStoreEvent::ObjectStoreGetObject {
                object_id: *object_id,
                result: res.clone(),
            });
        res
    }

    /// The object must be present in store by normal process we used to backfill store in init
    /// We dont download if not present
    fn get_object_by_key(
        &self,
        object_id: &ObjectID,
        version: VersionNumber,
    ) -> Result<Option<Object>, SuiError> {
        let res = Ok(self
            .storage
            .live_objects_store
            .get(object_id)
            .and_then(|obj| {
                if obj.version() == version {
                    Some(obj.clone())
                } else {
                    None
                }
            }));

        self.exec_store_events
            .lock()
            .expect("Unable to lock events list")
            .push(ExecutionStoreEvent::ObjectStoreGetObjectByKey {
                object_id: *object_id,
                version,
                result: res.clone(),
            });

        res
    }
}

impl ObjectStore for &mut LocalExec {
    fn get_object(&self, object_id: &ObjectID) -> Result<Option<Object>, SuiError> {
        // Recording event here will be double-counting since its already recorded in the get_module fn
        (**self).get_object(object_id)
    }

    fn get_object_by_key(
        &self,
        object_id: &ObjectID,
        version: VersionNumber,
    ) -> Result<Option<Object>, SuiError> {
        // Recording event here will be double-counting since its already recorded in the get_module fn
        (**self).get_object_by_key(object_id, version)
    }
}

impl GetModule for LocalExec {
    type Error = LocalExecError;
    type Item = CompiledModule;

    fn get_module_by_id(&self, id: &ModuleId) -> anyhow::Result<Option<Self::Item>, Self::Error> {
        let res = get_module_by_id(self, id).map_err(|e| e.into());

        self.exec_store_events
            .lock()
            .expect("Unable to lock events list")
            .push(ExecutionStoreEvent::GetModuleGetModuleByModuleId {
                id: id.clone(),
                result: res.clone(),
            });
        res
    }
}

// <--------------------- Util functions ----------------------->

pub fn get_vm(
    protocol_config: &ProtocolConfig,
    expensive_safety_check_config: ExpensiveSafetyCheckConfig,
) -> Result<Arc<adapter::MoveVM>, LocalExecError> {
    let native_functions = sui_move_natives::all_natives(/* disable silent */ false);
    let move_vm = Arc::new(
        adapter::new_move_vm(
            native_functions.clone(),
            protocol_config,
            expensive_safety_check_config.enable_move_vm_paranoid_checks(),
        )
        .expect("We defined natives to not fail here"),
    );
    Ok(move_vm)
}

fn extract_epoch_and_version(ev: SuiEvent) -> Result<(u64, u64), LocalExecError> {
    if let serde_json::Value::Object(w) = ev.parsed_json {
        let epoch = u64::from_str(&w["epoch"].to_string().replace('\"', "")).unwrap();
        let version = u64::from_str(&w["protocol_version"].to_string().replace('\"', "")).unwrap();
        return Ok((epoch, version));
    }

    Err(LocalExecError::UnexpectedEventFormat { event: ev })
}

async fn prep_network(
    objects: &[Object],
    reference_gas_price: u64,
    executed_epoch: u64,
    epoch_start_timestamp: u64,
    protocol_config: &ProtocolConfig,
) -> (Arc<AuthorityState>, Arc<AuthorityPerEpochStore>) {
    let authority_state = authority_state(protocol_config, objects, reference_gas_price).await;
    let epoch_store = create_epoch_store(
        &authority_state,
        reference_gas_price,
        executed_epoch,
        epoch_start_timestamp,
        protocol_config.version.as_u64(),
    )
    .await;

    (authority_state, epoch_store)
}

async fn authority_state(
    protocol_config: &ProtocolConfig,
    objects: &[Object],
    reference_gas_price: u64,
) -> Arc<AuthorityState> {
    // Initiaize some network
    TestAuthorityBuilder::new()
        .with_protocol_config(protocol_config.clone())
        .with_reference_gas_price(reference_gas_price)
        .with_starting_objects(objects)
        .build()
        .await
}

async fn create_epoch_store(
    authority_state: &Arc<AuthorityState>,
    reference_gas_price: u64,
    executed_epoch: u64,
    epoch_start_timestamp: u64,
    protocol_version: u64,
) -> Arc<AuthorityPerEpochStore> {
    let sys_state = EpochStartSystemState::new_v1(
        executed_epoch,
        protocol_version,
        reference_gas_price,
        false,
        epoch_start_timestamp,
        ONE_DAY_MS,
        vec![], // TODO: add validators
    );

    let path = {
        let dir = std::env::temp_dir();
        let store_base_path = dir.join(format!("DB_{:?}", ObjectID::random()));
        std::fs::create_dir(&store_base_path).unwrap();
        store_base_path
    };

    let epoch_start_config = EpochStartConfiguration::new(sys_state, CheckpointDigest::random());

    let registry = Registry::new();
    let cache_metrics = Arc::new(ResolverMetrics::new(&registry));
    let signature_verifier_metrics = SignatureVerifierMetrics::new(&registry);
    let mut committee = authority_state.committee_store().get_latest_committee();

    // Overwrite the epoch so it matches this TXs
    committee.epoch = executed_epoch;

    let name = committee.names().next().unwrap();
    AuthorityPerEpochStore::new(
        *name,
        Arc::new(committee.clone()),
        &path,
        None,
        EpochMetrics::new(&registry),
        epoch_start_config,
        authority_state.database.clone(),
        cache_metrics,
        signature_verifier_metrics,
        &ExpensiveSafetyCheckConfig::default(),
    )
}
