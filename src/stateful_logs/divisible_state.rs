use std::path::Path;
use std::sync::Arc;
use atlas_common::channel;
use atlas_common::globals::ReadOnly;
use atlas_common::error::*;
use atlas_common::ordering::SeqNo;
use atlas_common::persistentdb::KVDB;
use atlas_core::ordering_protocol::loggable::{OrderProtocolPersistenceHelper, PersistentOrderProtocolTypes, PProof};
use atlas_core::ordering_protocol::networking::serialize::{OrderingProtocolMessage, PermissionedOrderingProtocolMessage};
use atlas_core::ordering_protocol::{DecisionMetadata, ProtocolMessage};
use atlas_core::persistent_log::{DivisibleStateLog, OperationMode, OrderingProtocolLog, PersistableStateTransferProtocol, PersistentDecisionLog};
use atlas_core::smr::networking::serialize::DecisionLogMessage;
use atlas_core::smr::smr_decision_log::{DecisionLogPersistenceHelper, DecLog, DecLogMetadata, LoggingDecision, ShareableMessage};
use atlas_core::state_transfer::networking::serialize::StateTransferMessage;
use atlas_smr_application::app::UpdateBatch;
use atlas_smr_application::ExecutorHandle;
use atlas_smr_application::serialize::ApplicationData;
use atlas_smr_application::state::divisible_state::DivisibleState;
use crate::{PersistentLog, PersistentLogMode, PersistentLogModeTrait, worker};
use crate::worker::{COLUMN_FAMILY_OTHER, COLUMN_FAMILY_PROOFS, PersistentLogWorker, PersistentLogWorkerHandle, PersistentLogWriteStub};
use crate::worker::divisible_state_worker::{DivStatePersistentLogWorker, PersistentDivStateHandle, PersistentDivStateStub};

/// The message containing the information necessary to persist the most recently received
/// State parts
pub enum DivisibleStateMessage<S: DivisibleState> {
    Parts(Vec<Arc<ReadOnly<S::StatePart>>>),
    Descriptor(S::StateDescriptor),
    PartsAndDescriptor(Vec<Arc<ReadOnly<S::StatePart>>>, S::StateDescriptor),
    DeletePart(S::PartDescription),
}


pub struct DivisibleStatePersistentLog<S: DivisibleState, D: ApplicationData,
    OPM: OrderingProtocolMessage<D>,
    POPT: PersistentOrderProtocolTypes<D, OPM>,
    LS: DecisionLogMessage<D, OPM, POPT>,
    POP: PermissionedOrderingProtocolMessage, STM: StateTransferMessage> {
    request_tx: Arc<PersistentDivStateHandle<S>>,

    inner_log: PersistentLog<D, OPM, POPT, LS, POP, STM>,
}

impl<S, D, OPM, POPT, LS, POP, STM> DivisibleStatePersistentLog<S, D, OPM, POPT, LS, POP, STM>
    where S: DivisibleState + 'static,
          D: ApplicationData + 'static,
          OPM: OrderingProtocolMessage<D> + 'static,
          POPT: PersistentOrderProtocolTypes<D, OPM> + 'static,
          LS: DecisionLogMessage<D, OPM, POPT> + 'static,
          POP: PermissionedOrderingProtocolMessage + 'static,
          STM: StateTransferMessage + 'static
{
    fn init_div_log<K, T, POS, PSP, DLPH>(executor: ExecutorHandle<D>, db_path: K) -> Result<Self>
        where
            K: AsRef<Path>,
            T: PersistentLogModeTrait,
            POS: OrderProtocolPersistenceHelper<D, OPM, POPT> + Send + 'static,
            PSP: PersistableStateTransferProtocol + Send + 'static,
            DLPH: DecisionLogPersistenceHelper<D, OPM, POPT, LS> + 'static {
        let mut message_types = POS::message_types();

        let mut prefixes = vec![COLUMN_FAMILY_OTHER, COLUMN_FAMILY_PROOFS];

        prefixes.append(&mut message_types);

        let log_mode = T::init_persistent_log(executor);

        let mut response_txs = vec![];

        match &log_mode {
            PersistentLogMode::Strict(handle) => response_txs.push(handle.logger_tx().clone()),
            _ => {}
        }

        let kvdb = KVDB::new(db_path, prefixes)?;

        let (tx, rx) = channel::new_bounded_sync(1024);

        let worker = PersistentLogWorker::<D, OPM, POPT, POP, LS, PSP, POS, DLPH>::new(rx, response_txs, kvdb.clone());

        let (state_tx, state_rx) = channel::new_bounded_sync(10);

        let worker = DivStatePersistentLogWorker::<S, D, OPM, POPT, LS, POP, POS, PSP, DLPH>::new(state_rx, worker, kvdb.clone())?;

        match &log_mode {
            PersistentLogMode::Strict(_) | PersistentLogMode::Optimistic => {
                std::thread::Builder::new().name(format!("Persistent log Worker #1"))
                    .spawn(move || {
                        worker.work();
                    }).unwrap();
            }
            _ => {}
        }

        let persistent_log_write_stub = PersistentLogWriteStub { tx };

        let worker_handle = Arc::new(PersistentLogWorkerHandle::new(vec![persistent_log_write_stub]));

        let init_log = PersistentLog {
            persistency_mode: log_mode,
            worker_handle,
            p: Default::default(),
            db: kvdb,
        };

        let persistent_div_state = PersistentDivStateStub { tx: state_tx };

        let worker_handle = Arc::new(PersistentDivStateHandle::new(vec![persistent_div_state]));

        Ok(Self {
            request_tx: worker_handle,
            inner_log: init_log,
        })
    }
}

impl<S, D, OPM, POPT, LS, POP, STM> OrderingProtocolLog<D, OPM> for DivisibleStatePersistentLog<S, D, OPM, POPT, LS, POP, STM>
    where S: DivisibleState + 'static,
          D: ApplicationData + 'static,
          OPM: OrderingProtocolMessage<D> + 'static,
          POPT: PersistentOrderProtocolTypes<D, OPM> + 'static,
          LS: DecisionLogMessage<D, OPM, POPT> + 'static,
          POP: PermissionedOrderingProtocolMessage + 'static,
          STM: StateTransferMessage + 'static
{
    fn write_committed_seq_no(&self, write_mode: OperationMode, seq: SeqNo) -> Result<()> {
        self.inner_log.write_committed_seq_no(write_mode, seq)
    }

    fn write_message(&self, write_mode: OperationMode, msg: ShareableMessage<ProtocolMessage<D, OPM>>) -> Result<()> {
        self.inner_log.write_message(write_mode, msg)
    }

    fn write_decision_metadata(&self, write_mode: OperationMode, metadata: DecisionMetadata<D, OPM>) -> Result<()> {
        self.inner_log.write_decision_metadata(write_mode, metadata)
    }

    fn write_invalidate(&self, write_mode: OperationMode, seq: SeqNo) -> Result<()> {
        self.inner_log.write_invalidate(write_mode, seq)
    }
}

impl<S, D, OPM, POPT, LS, POP, STM> PersistentDecisionLog<D, OPM, POPT, LS> for DivisibleStatePersistentLog<S, D, OPM, POPT, LS, POP, STM>
    where S: DivisibleState + 'static,
          D: ApplicationData + 'static,
          OPM: OrderingProtocolMessage<D> + 'static,
          POPT: PersistentOrderProtocolTypes<D, OPM> + 'static,
          LS: DecisionLogMessage<D, OPM, POPT> + 'static,
          POP: PermissionedOrderingProtocolMessage + 'static,
          STM: StateTransferMessage + 'static,
{
    fn checkpoint_received(&self, mode: OperationMode, seq: SeqNo) -> Result<()> {
        self.inner_log.checkpoint_received(mode, seq)
    }

    fn write_proof(&self, write_mode: OperationMode, proof: PProof<D, OPM, POPT>) -> Result<()> {
        self.inner_log.write_proof(write_mode, proof)
    }

    fn write_decision_log_metadata(&self, mode: OperationMode, log_metadata: DecLogMetadata<D, OPM, POPT, LS>) -> Result<()> {
        self.inner_log.write_decision_log_metadata(mode, log_metadata)
    }

    fn write_decision_log(&self, mode: OperationMode, log: DecLog<D, OPM, POPT, LS>) -> Result<()> {
        self.inner_log.write_decision_log(mode, log)
    }

    fn read_proof(&self, mode: OperationMode, seq: SeqNo) -> Result<Option<PProof<D, OPM, POPT>>> {
        todo!()
    }

    fn read_decision_log(&self, mode: OperationMode) -> Result<Option<DecLog<D, OPM, POPT, LS>>> {
        self.inner_log.read_decision_log(mode)
    }

    fn reset_log(&self, mode: OperationMode) -> Result<()> {
        self.inner_log.reset_log(mode)
    }

    fn wait_for_full_persistence(&self, batch: UpdateBatch<D::Request>, decision_logging: LoggingDecision) -> Result<Option<UpdateBatch<D::Request>>> {
        self.inner_log.wait_for_full_persistence(batch, decision_logging)
    }
}

impl<S, D, OPM, POPT, LS, POP, STM> DivisibleStateLog<S> for DivisibleStatePersistentLog<S, D, OPM, POPT, LS, POP, STM>
    where S: DivisibleState + 'static,
          D: ApplicationData + 'static,
          OPM: OrderingProtocolMessage<D> + 'static,
          POPT: PersistentOrderProtocolTypes<D, OPM> + 'static,
          LS: DecisionLogMessage<D, OPM, POPT> + 'static,
          POP: PermissionedOrderingProtocolMessage + 'static,
          STM: StateTransferMessage + 'static
{
    fn write_descriptor(&self, write_mode: OperationMode, checkpoint: S::StateDescriptor) -> Result<()> {
        match self.inner_log.persistency_mode {
            PersistentLogMode::Strict(_) | PersistentLogMode::Optimistic => {
                match write_mode {
                    OperationMode::NonBlockingSync(callback) => {
                        self.request_tx.queue_descriptor(checkpoint)?;
                    }
                    OperationMode::BlockingSync => {
                        todo!()
                    }
                }
            }
            PersistentLogMode::None => {}
        }

        Ok(())
    }

    fn delete_part(&self, write_mode: OperationMode, part: S::PartDescription) -> Result<()> {
        match self.inner_log.persistency_mode {
            PersistentLogMode::Strict(_) | PersistentLogMode::Optimistic => {
                match write_mode {
                    OperationMode::NonBlockingSync(callback) => {
                        self.request_tx.queue_delete_part(part)?;
                    }
                    OperationMode::BlockingSync => {
                        todo!()
                    }
                }
            }
            PersistentLogMode::None => {}
        }

        Ok(())
    }

    fn write_parts(&self, write_mode: OperationMode, parts: Vec<Arc<ReadOnly<S::StatePart>>>) -> Result<()> {
        match self.inner_log.persistency_mode {
            PersistentLogMode::Strict(_) | PersistentLogMode::Optimistic => {
                match write_mode {
                    OperationMode::NonBlockingSync(callback) => {
                        self.request_tx.queue_state_parts(parts).unwrap();
                    }
                    OperationMode::BlockingSync => {
                        todo!()
                    }
                }
            }
            PersistentLogMode::None => {}
        }

        Ok(())
    }

    fn write_parts_and_descriptor(&self, write_mode: OperationMode, descriptor: S::StateDescriptor, parts: Vec<Arc<ReadOnly<S::StatePart>>>) -> Result<()> {
        match self.inner_log.persistency_mode {
            PersistentLogMode::Strict(_) | PersistentLogMode::Optimistic => {
                match write_mode {
                    OperationMode::NonBlockingSync(callback) => {
                        self.request_tx.queue_descriptor_and_parts(descriptor, parts).unwrap();
                    }
                    OperationMode::BlockingSync => {
                        todo!()
                    }
                }
            }
            PersistentLogMode::None => {}
        }

        Ok(())
    }

    fn read_local_descriptor(&self) -> Result<Option<S::StateDescriptor>> {
        match self.inner_log.persistency_mode {
            PersistentLogMode::Strict(_) | PersistentLogMode::Optimistic => {
                worker::divisible_state_worker::read_latest_descriptor::<S>(&self.inner_log.db)
            }
            PersistentLogMode::None => {
                Ok(None)
            }
        }
    }

    fn read_local_part(&self, part: S::PartDescription) -> Result<Option<S::StatePart>> {
        match self.inner_log.persistency_mode {
            PersistentLogMode::Strict(_) | PersistentLogMode::Optimistic => {
                worker::divisible_state_worker::read_state_part::<S>(&self.inner_log.db, &part)
            }
            PersistentLogMode::None => {
                Ok(None)
            }
        }
    }
}

impl<S, D, OPM, POPT, LS, POP, STM> Clone for DivisibleStatePersistentLog<S, D, OPM, POPT, LS, POP, STM>
    where S: DivisibleState + 'static,
          D: ApplicationData + 'static,
          OPM: OrderingProtocolMessage<D> + 'static,
          POPT: PersistentOrderProtocolTypes<D, OPM> + 'static,
          LS: DecisionLogMessage<D, OPM, POPT> + 'static,
          POP: PermissionedOrderingProtocolMessage + 'static,
          STM: StateTransferMessage + 'static {
    fn clone(&self) -> Self {
        Self {
            request_tx: self.request_tx.clone(),
            inner_log: self.inner_log.clone(),
        }
    }
}
