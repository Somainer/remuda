//! Observation envelope submitted to the journal before `seq` assignment.

use remuda_protocol::{
    Completeness, EventId, HostId, Id, InstanceId, Knowledge, ObservationPayload,
    ObservationSource, Redaction, RunId, Timestamp, U64,
};

/// Raw native bytes stored before the observation commit; `protocol.md` §5.1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawPayload {
    /// Native bytes, usually one JSONL line.
    pub bytes: Vec<u8>,
    /// Media type recorded on `RawRef`.
    pub media_type: String,
    /// Redaction applied to this object.
    pub redaction: Redaction,
}

/// Observation minus journal-assigned `eventId`, `seq`, and `rawRef`.
#[derive(Debug, Clone, PartialEq)]
pub struct Envelope {
    /// Journal identity shared by every seq of this instance.
    pub journal_id: Id,
    /// Instance whose log this envelope belongs to.
    pub instance_id: InstanceId,
    /// Run this observation is attached to, if known.
    pub run_id: Option<RunId>,
    /// Host that observed the event.
    pub host_id: HostId,
    /// Process generation of the observing Node.
    pub process_generation: U64,
    /// Run generation, if a run is attached.
    pub run_generation: Option<U64>,
    /// Time the runtime observed the event.
    pub observed_at: Timestamp,
    /// Native event time when the source emitted one.
    pub native_at: Knowledge<Timestamp>,
    /// Driver, channel, and source cursor.
    pub source: ObservationSource,
    /// How completely this payload maps the native record.
    pub completeness: Completeness,
    /// Related prior event IDs.
    pub evidence_event_ids: Vec<EventId>,
    /// Discriminated payload.
    pub body: ObservationPayload,
    /// Native bytes to persist first; runtime-only facts may omit this.
    pub raw: Option<RawPayload>,
}

impl Envelope {
    /// Attach a JSONL line as the raw object for this envelope.
    pub fn with_jsonl_raw(mut self, bytes: Vec<u8>) -> Self {
        self.raw = Some(RawPayload {
            bytes,
            media_type: "application/jsonl; charset=utf-8".into(),
            redaction: Redaction::None,
        });
        self
    }
}
