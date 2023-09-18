use std::marker::PhantomData;
use std::ops::Deref;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use log::error;

use atlas_common::channel::{ChannelSyncRx, ChannelSyncTx, SendError};
use atlas_common::error::*;
use atlas_common::globals::ReadOnly;
use atlas_common::ordering::{Orderable, SeqNo};
use atlas_common::persistentdb::KVDB;
use atlas_communication::message::{Header, StoredMessage};
use atlas_core::ordering_protocol::{LoggableMessage, SerProof, SerProofMetadata, View};
use atlas_core::ordering_protocol::networking::serialize::{OrderingProtocolMessage, PermissionedOrderingProtocolMessage, StatefulOrderProtocolMessage};
use atlas_core::ordering_protocol::stateful_order_protocol::DecLog;
use atlas_core::persistent_log::{PersistableOrderProtocol, PersistableStateTransferProtocol};
use atlas_execution::serialize::ApplicationData;
use atlas_execution::state::divisible_state::DivisibleState;
use atlas_execution::state::monolithic_state::MonolithicState;

use crate::{CallbackType, ChannelMsg, DivisibleStateMessage, InstallState, PWMessage, ResponseMessage, serialize};

pub(super) mod monolithic_worker;
pub(super) mod divisible_state_worker;

///Latest checkpoint made by the execution
const LATEST_STATE: &str = "latest_state";

///First sequence number (committed) since the last checkpoint
const FIRST_SEQ: &str = "first_seq";
///Last sequence number (committed) since the last checkpoint
const LATEST_SEQ: &str = "latest_seq";
///Latest known view sequence number
const LATEST_VIEW_SEQ: &str = "latest_view_seq";

/// The default column family for the persistent logging
pub(super) const COLUMN_FAMILY_OTHER: &str = "other";
pub(super) const COLUMN_FAMILY_PROOFS: &str = "proof_metadata";
pub(super) const COLUMN_FAMILY_STATE: &str = "state";


/// A handle for all of the persistent workers.
/// Handles task distribution and load balancing across the
/// workers
pub struct PersistentLogWorkerHandle<D, OPM: OrderingProtocolMessage<D>, SOPM: StatefulOrderProtocolMessage<D, OPM>, POP: PermissionedOrderingProtocolMessage> {
    round_robin_counter: AtomicUsize,
    tx: Vec<PersistentLogWriteStub<D, OPM, SOPM, POP>>,
}


///A stub that is only useful for writing to the persistent log
#[derive(Clone)]
pub struct PersistentLogWriteStub<D, OPM: OrderingProtocolMessage<D>, SOPM: StatefulOrderProtocolMessage<D, OPM>, POP: PermissionedOrderingProtocolMessage> {
    pub(crate) tx: ChannelSyncTx<ChannelMsg<D, OPM, SOPM, POP>>,
}

/// A writing stub for divisible state objects
#[derive(Clone)]
pub struct PersistentDivisibleStateStub<S: DivisibleState> {
    pub(crate) tx: ChannelSyncTx<DivisibleStateMessage<S>>,
}

impl<D, OPM, SOPM, POP> PersistentLogWorkerHandle<D, OPM, SOPM, POP>
    where OPM: OrderingProtocolMessage<D> + 'static,
          SOPM: StatefulOrderProtocolMessage<D, OPM> + 'static,
          POP: PermissionedOrderingProtocolMessage + 'static {
    pub fn new(tx: Vec<PersistentLogWriteStub<D, OPM, SOPM, POP>>) -> Self {
        Self { round_robin_counter: AtomicUsize::new(0), tx }
    }
}


///A worker for the persistent logging
pub struct PersistentLogWorker<D, OPM, SOPM, POPM, POP, PSP>
    where D: ApplicationData + 'static,
          OPM: OrderingProtocolMessage<D> + 'static,
          SOPM: StatefulOrderProtocolMessage<D, OPM> + 'static,
          POPM: PermissionedOrderingProtocolMessage + 'static,
          POP: PersistableOrderProtocol<D, OPM, SOPM> + 'static,
          PSP: PersistableStateTransferProtocol + 'static {
    request_rx: ChannelSyncRx<ChannelMsg<D, OPM, SOPM, POPM>>,

    response_txs: Vec<ChannelSyncTx<ResponseMessage>>,

    db: KVDB,

    phantom: PhantomData<(D, POP, PSP)>,
}


impl<D, OPM: OrderingProtocolMessage<D>, SOPM: StatefulOrderProtocolMessage<D, OPM>, POP: PermissionedOrderingProtocolMessage> PersistentLogWorkerHandle<D, OPM, SOPM, POP> {
    /// Employ a simple round robin load distribution
    fn next_worker(&self) -> &PersistentLogWriteStub<D, OPM, SOPM, POP> {
        let counter = self.round_robin_counter.fetch_add(1, Ordering::Relaxed);

        self.tx.get(counter % self.tx.len()).unwrap()
    }

    fn translate_error<V, T>(result: std::result::Result<V, SendError<T>>) -> Result<V> {
        match result {
            Ok(v) => {
                Ok(v)
            }
            Err(err) => {
                Err(Error::simple_with_msg(ErrorKind::MsgLogPersistent, format!("{:?}", err).as_str()))
            }
        }
    }

    pub(super) fn register_callback_receiver(&self, receiver: ChannelSyncTx<ResponseMessage>) -> Result<()> {
        for write_stub in &self.tx {
            Self::translate_error(write_stub.send((PWMessage::RegisterCallbackReceiver(receiver.clone()), None)))?;
        }

        Ok(())
    }

    pub(super) fn queue_invalidate(&self, seq_no: SeqNo, callback: Option<CallbackType>) -> Result<()> {
        Self::translate_error(self.next_worker().send((PWMessage::Invalidate(seq_no), callback)))
    }

    pub(crate) fn queue_committed(&self, seq_no: SeqNo, callback: Option<CallbackType>) -> Result<()> {
        Self::translate_error(self.next_worker().send((PWMessage::Committed(seq_no), callback)))
    }

    pub(super) fn queue_proof_metadata(&self, metadata: SerProofMetadata<D, OPM>, callback: Option<CallbackType>) -> Result<()> {
        Self::translate_error(self.next_worker().send((PWMessage::ProofMetadata(metadata), callback)))
    }

    pub(super) fn queue_view_number(&self, view: View<POP>, callback: Option<CallbackType>) -> Result<()> {
        Self::translate_error(self.next_worker().send((PWMessage::View(view), callback)))
    }

    pub(super) fn queue_message(&self, message: Arc<ReadOnly<StoredMessage<LoggableMessage<D, OPM>>>>,
                                callback: Option<CallbackType>) -> Result<()> {
        Self::translate_error(self.next_worker().send((PWMessage::Message(message), callback)))
    }

    pub(super) fn queue_install_state(&self, install_state: InstallState<D, OPM, SOPM, POP>, callback: Option<CallbackType>) -> Result<()> {
        Self::translate_error(self.next_worker().send((PWMessage::InstallState(install_state), callback)))
    }

    pub(super) fn queue_proof(&self, proof: SerProof<D, OPM>, callback: Option<CallbackType>) -> Result<()> {
        Self::translate_error(self.next_worker().send((PWMessage::Proof(proof), callback)))
    }
}


impl<D, OPM, SOPM, POPM, PS, PSP> PersistentLogWorker<D, OPM, SOPM, POPM, PS, PSP>
    where D: ApplicationData + 'static,
          OPM: OrderingProtocolMessage<D> + 'static,
          SOPM: StatefulOrderProtocolMessage<D, OPM> + 'static,
          POPM: PermissionedOrderingProtocolMessage + 'static,
          PS: PersistableOrderProtocol<D, OPM, SOPM> + 'static,
          PSP: PersistableStateTransferProtocol + 'static {
    pub fn new(request_rx: ChannelSyncRx<ChannelMsg<D, OPM, SOPM, POPM>>,
               response_txs: Vec<ChannelSyncTx<ResponseMessage>>,
               db: KVDB) -> Self {
        Self { request_rx, response_txs, db, phantom: Default::default() }
    }

    fn work_iteration(&mut self) -> Result<()> {
        let (request, callback) = match self.request_rx.recv() {
            Ok((request, callback)) => (request, callback),
            Err(err) => {
                error!("{:?}", err);

                return Err(Error::wrapped(ErrorKind::MsgLog, err));
            }
        };

        let response = self.exec_req(request);

        if let Some(callback) = callback {
            //If we have a callback to call with the response, then call it
            // (callback)(response);
        } else {
            //If not, then deliver it down the response_txs
            match response {
                Ok(response) => {
                    for ele in &self.response_txs {
                        if let Err(err) = ele.send(response.clone()) {
                            error!("Failed to deliver response to log. {:?}", err);

                            return Err(Error::wrapped(ErrorKind::MsgLog, err));
                        }
                    }
                }
                Err(err) => {
                    error!("Failed to execute persistent log request because {:?}", err);

                    return Err(Error::wrapped(ErrorKind::MsgLog, err));
                }
            }
        }

        Ok(())
    }

    /// Work loop of this worker
    pub(super) fn work(mut self) {
        loop {
            if let Err(err) = self.work_iteration() {
                error!("Failed to execute persistent log request because {:?}", err);

                break;
            }
        }
    }

    fn exec_req(&mut self, message: PWMessage<D, OPM, SOPM, POPM>) -> Result<ResponseMessage> {
        Ok(match message {
            PWMessage::View(view) => {
                write_latest_view::<POPM>(&self.db, &view)?;

                ResponseMessage::ViewPersisted(view.sequence_number())
            }
            PWMessage::Committed(seq) => {
                write_latest_seq_no(&self.db, seq)?;

                ResponseMessage::CommittedPersisted(seq)
            }
            PWMessage::Message(msg) => {
                write_message::<D, OPM, SOPM, PS>(&self.db, &msg)?;

                let seq = msg.message().sequence_number();

                ResponseMessage::WroteMessage(seq, msg.header().digest().clone())
            }
            PWMessage::Invalidate(seq) => {
                invalidate_seq::<D, OPM, SOPM, PS>(&self.db, seq)?;

                ResponseMessage::InvalidationPersisted(seq)
            }
            PWMessage::InstallState(state) => {
                let seq_no = state.1.sequence_number();

                write_state::<D, OPM, SOPM, POPM, PS>(&self.db, state)?;

                ResponseMessage::InstalledState(seq_no)
            }
            PWMessage::Proof(proof) => {
                let seq_no = proof.sequence_number();

                write_proof::<D, OPM, SOPM, PS>(&self.db, &proof)?;

                ResponseMessage::Proof(seq_no)
            }
            PWMessage::RegisterCallbackReceiver(receiver) => {
                self.response_txs.push(receiver);

                ResponseMessage::RegisteredCallback
            }
            PWMessage::ProofMetadata(metadata) => {
                let seq = metadata.sequence_number();

                write_proof_metadata::<D, OPM, SOPM, PS>(&self.db, &metadata)?;

                ResponseMessage::WroteMetadata(seq)
            }
        })
    }
}

/// Read the latest state from the persistent log
pub(super) fn read_latest_state<D, OPM: OrderingProtocolMessage<D>, SOPM: StatefulOrderProtocolMessage<D, OPM>, POPM: PermissionedOrderingProtocolMessage, PS: PersistableOrderProtocol<D, OPM, SOPM>>(db: &KVDB) -> Result<Option<InstallState<D, OPM, SOPM, POPM>>> {
    let view_seq = read_latest_view_seq::<POPM>(db)?;

    if let None = view_seq {
        return Ok(None);
    }

    let dec_log = read_decision_log::<D, OPM, SOPM, PS>(db)?;

    if let None = dec_log {
        return Ok(None);
    }

    Ok(Some((view_seq.unwrap(), dec_log.unwrap())))
}

fn read_decision_log<D, OPM: OrderingProtocolMessage<D>, SOPM: StatefulOrderProtocolMessage<D, OPM>, PS: PersistableOrderProtocol<D, OPM, SOPM>>(db: &KVDB) -> Result<Option<DecLog<D, OPM, SOPM>>> {
    let first_seq = db.get(COLUMN_FAMILY_OTHER, FIRST_SEQ)?;
    let last_seq = db.get(COLUMN_FAMILY_OTHER, LATEST_SEQ)?;

    let start_seq = if let Some(first_seq) = first_seq {
        serialize::read_seq(first_seq.as_slice())?
    } else {
        return Ok(None);
    };

    let end_seq = if let Some(end_seq) = last_seq {
        serialize::read_seq(end_seq.as_slice())?
    } else {
        return Ok(None);
    };

    let start_point = serialize::make_seq(start_seq)?;
    let end_point = serialize::make_seq(end_seq.next())?;

    let mut proofs = Vec::new();

    for result in db.iter_range(COLUMN_FAMILY_PROOFS, Some(start_point.as_slice()), Some(end_point.as_slice()))? {
        let (key, value) = result?;

        let seq = serialize::read_seq(&*key)?;

        let proof_metadata = serialize::deserialize_proof_metadata::<&[u8], D, OPM>(&mut &*value)?;

        let messages = read_messages_for_seq::<D, OPM, SOPM, PS>(db, seq)?;

        let proof = PS::init_proof_from(proof_metadata, messages);

        proofs.push(proof);
    }

    Ok(Some(PS::init_dec_log(proofs)))
}

fn read_messages_for_seq<D, OPM: OrderingProtocolMessage<D>, SOPM: StatefulOrderProtocolMessage<D, OPM>, PS: PersistableOrderProtocol<D, OPM, SOPM>>(db: &KVDB, seq: SeqNo) -> Result<Vec<StoredMessage<LoggableMessage<D, OPM>>>> {
    let start_seq = serialize::make_message_key(seq, None)?;
    let end_seq = serialize::make_message_key(seq.next(), None)?;

    let mut messages = Vec::new();

    for column_family in PS::message_types() {
        for result in db.iter_range(column_family, Some(start_seq.as_slice()), Some(end_seq.as_slice()))? {
            let (key, value) = result?;

            let header = Header::deserialize_from(&mut &(*value)[..Header::LENGTH])?;

            let message = serialize::deserialize_message::<&[u8], D, OPM>(&mut &(*value)[Header::LENGTH..]).unwrap();

            messages.push(StoredMessage::new(header, message));
        }
    }

    Ok(messages)
}

fn read_latest_view_seq<POP: PermissionedOrderingProtocolMessage>(db: &KVDB) -> Result<Option<View<POP>>> {
    let mut result = db.get(COLUMN_FAMILY_OTHER, LATEST_VIEW_SEQ)?;

    let option = if let Some(mut result) = result {
        Some(serialize::deserialize_view::<&[u8], POP>(&mut result.as_slice())?)
    } else {
        None
    };

    if let Some(seq) = option {
        return Ok(Some(seq));
    } else {
        return Ok(None);
    }
}


/// Writes a given state to the persistent log
pub(super) fn write_state<D: ApplicationData,
    OPM: OrderingProtocolMessage<D>,
    SOPM: StatefulOrderProtocolMessage<D, OPM>,
    POP: PermissionedOrderingProtocolMessage,
    PS: PersistableOrderProtocol<D, OPM, SOPM>>(
    db: &KVDB, (view, dec_log): InstallState<D, OPM, SOPM, POP>,
) -> Result<()> {
    write_latest_view::<POP>(db, &view)?;

    write_dec_log::<D, OPM, SOPM, PS>(db, &dec_log)
}

pub(super) fn write_latest_view<POP: PermissionedOrderingProtocolMessage>(db: &KVDB, view_seq_no: &View<POP>) -> Result<()> {
    let mut res = Vec::new();

    let f_seq_no = serialize::serialize_view::<Vec<u8>, POP>(&mut res, view_seq_no)?;

    db.set(COLUMN_FAMILY_OTHER, LATEST_VIEW_SEQ, &res[..])
}

pub(super) fn write_latest_seq_no(db: &KVDB, seq_no: SeqNo) -> Result<()> {
    let mut f_seq_no = serialize::make_seq(seq_no)?;

    if !db.exists(COLUMN_FAMILY_OTHER, FIRST_SEQ)? {
        db.set(COLUMN_FAMILY_OTHER, FIRST_SEQ, &f_seq_no[..])?;
    }

    db.set(COLUMN_FAMILY_OTHER, LATEST_SEQ, &f_seq_no[..])
}

pub(super) fn write_dec_log<D, OPM: OrderingProtocolMessage<D>, SOPM: StatefulOrderProtocolMessage<D, OPM>, PS: PersistableOrderProtocol<D, OPM, SOPM>>(db: &KVDB, dec_log: &DecLog<D, OPM, SOPM>) -> Result<()> {
    write_latest_seq_no(db, dec_log.sequence_number())?;

    for proof_ref in PS::decompose_dec_log(dec_log) {
        write_proof::<D, OPM, SOPM, PS>(db, proof_ref)?;
    }

    Ok(())
}

pub(super) fn write_proof<D, OPM: OrderingProtocolMessage<D>, SOPM: StatefulOrderProtocolMessage<D, OPM>, PS: PersistableOrderProtocol<D, OPM, SOPM>>(db: &KVDB, proof: &SerProof<D, OPM>) -> Result<()> {
    let (proof_metadata, messages) = PS::decompose_proof(proof);

    write_proof_metadata::<D, OPM, SOPM, PS>(db, proof_metadata)?;

    for message in messages {
        write_message::<D, OPM, SOPM, PS>(db, message)?;
    }

    Ok(())
}

pub(super) fn write_message<D, OPM: OrderingProtocolMessage<D>, SOPM: StatefulOrderProtocolMessage<D, OPM>, PS: PersistableOrderProtocol<D, OPM, SOPM>>(db: &KVDB, message: &StoredMessage<LoggableMessage<D, OPM>>) -> Result<()> {
    let mut buf = Vec::with_capacity(Header::LENGTH + message.header().payload_length());

    message.header().serialize_into(&mut buf[..Header::LENGTH]).unwrap();

    serialize::serialize_message::<&mut [u8], D, OPM>(&mut &mut buf[Header::LENGTH..], message.message())?;

    let msg_seq = message.message().sequence_number();

    let key = serialize::make_message_key(msg_seq, Some(message.header().from()))?;

    let column_family = PS::get_type_for_message(message.message())?;

    db.set(column_family, key, buf)
}

pub(super) fn write_proof_metadata<D, OPM: OrderingProtocolMessage<D>, SOPM: StatefulOrderProtocolMessage<D, OPM>, PS: PersistableOrderProtocol<D, OPM, SOPM>>(db: &KVDB, proof_metadata: &SerProofMetadata<D, OPM>) -> Result<()> {
    let seq_no = serialize::make_seq(proof_metadata.sequence_number())?;

    let mut proof_vec = Vec::new();

    let _ = serialize::serialize_proof_metadata::<Vec<u8>, D, OPM>(&mut proof_vec, proof_metadata)?;

    db.set(COLUMN_FAMILY_PROOFS, seq_no, &proof_vec[..])
}

fn delete_proofs_between<D, OPM: OrderingProtocolMessage<D>, SOPM: StatefulOrderProtocolMessage<D, OPM>, PS: PersistableOrderProtocol<D, OPM, SOPM>>(db: &KVDB, start: SeqNo, end: SeqNo) -> Result<()> {
    let start = serialize::make_seq(start)?;
    let end = serialize::make_seq(end)?;

    for column_family in PS::message_types() {
        //Erase all of the messages
        db.erase_range(column_family, start.as_slice(), end.as_slice())?;
    }

    // Erase all of the proof metadata
    db.erase_range(COLUMN_FAMILY_PROOFS, start.as_slice(), end.as_slice())?;

    Ok(())
}

pub(super) fn invalidate_seq<D, OPM: OrderingProtocolMessage<D>, SOPM: StatefulOrderProtocolMessage<D, OPM>, PS: PersistableOrderProtocol<D, OPM, SOPM>>(db: &KVDB, seq: SeqNo) -> Result<()> {
    delete_all_msgs_for_seq::<D, OPM, SOPM, PS>(db, seq)?;
    delete_all_proof_metadata_for_seq(db, seq)?;

    Ok(())
}

///Delete all msgs relating to a given sequence number
fn delete_all_msgs_for_seq<D, OPM: OrderingProtocolMessage<D>, SOPM: StatefulOrderProtocolMessage<D, OPM>, PS: PersistableOrderProtocol<D, OPM, SOPM>>(db: &KVDB, msg_seq: SeqNo) -> Result<()> {
    let mut start_key =
        serialize::make_message_key(msg_seq, None)?;
    let mut end_key =
        serialize::make_message_key(msg_seq.next(), None)?;

    for column_family in PS::message_types() {
        //Erase all of the messages
        db.erase_range(column_family, start_key.as_slice(), end_key.as_slice())?;
    }

    Ok(())
}

fn delete_all_proof_metadata_for_seq(db: &KVDB, seq: SeqNo) -> Result<()> {
    let seq = serialize::make_seq(seq)?;

    db.erase(COLUMN_FAMILY_PROOFS, &seq)?;

    Ok(())
}


impl<D, OPM: OrderingProtocolMessage<D>, SOPM: StatefulOrderProtocolMessage<D, OPM>, POP: PermissionedOrderingProtocolMessage> Deref for PersistentLogWriteStub<D, OPM, SOPM, POP> {
    type Target = ChannelSyncTx<ChannelMsg<D, OPM, SOPM, POP>>;

    fn deref(&self) -> &Self::Target {
        &self.tx
    }
}
