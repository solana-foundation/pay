//! Session-aware streaming metering for proxied HTTP responses.
//!
//! The proxy uses this module to observe provider streams, rate the cumulative
//! usage through the YAML-derived session meter, and apply backpressure until
//! the client has committed a voucher for the newly delivered watermark.

use std::error::Error as StdError;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Bytes;
use futures_util::{Stream, StreamExt};
use pay_types::metering::{BillingUnit, MeterDimension, MeterDirection, Metering};
use serde_json::Value;

use crate::server::completion_stream::MeteredSettlement;
use crate::server::gate::SessionForward;
use crate::server::metering::{
    RequestProperties, provider_token_quantity, quota_tokens_per_unit, resolve_variant,
};
use crate::server::session::{DelegatedUsageAuthorization, SessionMpp};
use crate::server::session_metering::{
    GateMode, Result as MeteringResult, SessionGateDecision, SessionMeterDimension,
    SessionMeterSpec, SessionMeteringContext, SessionUsageGate, StablecoinSettlement,
    UsageObservation, is_response_measured, spec_from_metering,
};

const COMMIT_WAIT_TIMEOUT: Duration = Duration::from_secs(30);
const COMMIT_POLL_INTERVAL: Duration = Duration::from_millis(50);
const LIFECYCLE_TOUCH_INTERVAL: Duration = Duration::from_secs(30);
const MAX_NDJSON_RECORD_BYTES: usize = 1024 * 1024;

pub type BoxError = Box<dyn StdError + Send + Sync>;

/// Session data attached by the payment middleware to a paid upstream retry.
#[derive(Clone)]
pub struct SessionStreamContext {
    session_mpp: Arc<SessionMpp>,
    session_id: String,
    baseline_base_units: u64,
}

impl SessionStreamContext {
    pub fn new(
        session_mpp: Arc<SessionMpp>,
        session_id: impl Into<String>,
        baseline_base_units: u64,
    ) -> Self {
        Self {
            session_mpp,
            session_id: session_id.into(),
            baseline_base_units,
        }
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn baseline_base_units(&self) -> u64 {
        self.baseline_base_units
    }

    pub fn committed_base_units(&self) -> u64 {
        self.session_mpp
            .committed_watermark(&self.session_id)
            .unwrap_or(self.baseline_base_units)
            .max(self.baseline_base_units)
    }

    pub fn settlement(&self) -> StablecoinSettlement {
        StablecoinSettlement::new(self.session_mpp.decimals())
    }

    pub fn min_voucher_delta(&self) -> u64 {
        self.session_mpp.min_voucher_delta()
    }

    pub fn touch_channel_unconfirmed(&self) {
        self.session_mpp
            .touch_channel_unconfirmed(self.session_id.clone());
    }
}

#[derive(Debug, Clone, Default)]
pub struct SessionUsageHints {
    quota_units: Vec<QuotaUnitHint>,
    /// Request body size: an upper bound on prompt tokens when the provider
    /// never reports input usage.
    request_body_bytes: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
struct QuotaUnitHint {
    direction: MeterDirection,
    tokens_per_unit: u64,
}

impl SessionUsageHints {
    pub fn from_metering(
        metering: &Metering,
        variant_hint: Option<&str>,
        request_properties: &RequestProperties,
    ) -> Self {
        let dimensions = select_meter_dimensions(metering, variant_hint).unwrap_or(&[]);
        let quota_units = dimensions
            .iter()
            .filter_map(|dimension| {
                if dimension.unit != BillingUnit::QuotaUnits {
                    return None;
                }
                let tokens_per_unit = quota_tokens_per_unit(dimension, request_properties)?;
                Some(QuotaUnitHint {
                    direction: dimension.direction,
                    tokens_per_unit,
                })
            })
            .collect();

        Self {
            quota_units,
            request_body_bytes: request_properties.body_size,
        }
    }

    fn tokens_per_quota_unit(&self, direction: MeterDirection) -> Option<u64> {
        self.quota_units
            .iter()
            .find(|hint| hint.direction == direction)
            .map(|hint| hint.tokens_per_unit)
    }
}

/// Builds a stream meter from the current YAML metering config.
pub fn meter_from_config(
    metering: &Metering,
    request_properties: &RequestProperties,
    variant_hint: Option<&str>,
    context: SessionStreamContext,
) -> MeteringResult<Option<SessionStreamMeter>> {
    let spec = spec_from_metering(
        metering,
        SessionMeteringContext::new()
            .with_request_properties(request_properties)
            .with_optional_variant_hint(variant_hint),
    )?;

    if !has_stream_observable_dimension(&spec) {
        return Ok(None);
    }

    let hints = SessionUsageHints::from_metering(metering, variant_hint, request_properties);
    SessionStreamMeter::new(spec, hints, context).map(Some)
}

pub struct SessionStreamMeter {
    context: SessionStreamContext,
    gate: SessionUsageGate,
    accumulator: StreamUsageAccumulator,
    current: UsageObservation,
    next_lifecycle_touch: Instant,
}

/// Server-authorized meter for a delegated session response stream.
///
/// The meter owns the session's capacity lease for the lifetime of the body.
/// Each cumulative usage increase is signed and persisted before the bytes
/// that exposed it are released to the client.
pub struct DelegatedSessionStreamMeter {
    forward: SessionForward,
    gate: SessionUsageGate,
    accumulator: StreamUsageAccumulator,
    current: UsageObservation,
    priced_context_length: Option<u64>,
    clamp_reprice_to_authorized: bool,
    authorization_exhausted: bool,
    next_lifecycle_touch: Instant,
    observer: Option<MeteredSettlement>,
    /// This stream's charge in progress. It runs detached, so a cancelled
    /// stream still learns whether it landed before charging again.
    in_flight: Option<InFlightCharge>,
}

/// One delegated charge for this stream, running to completion on its own
/// task whatever happens to the stream that started it.
struct InFlightCharge {
    amount: u64,
    commit: tokio::task::JoinHandle<crate::Result<DelegatedUsageAuthorization>>,
}

impl DelegatedSessionStreamMeter {
    pub fn from_forward(forward: SessionForward) -> Result<Self, BoxError> {
        let plan = forward.metered_plan().ok_or_else(|| {
            box_error(std::io::Error::other(
                "delegated stream forward is missing its settlement plan",
            ))
        })?;
        if !stream_honors_meters(&plan.metering) {
            return Err(box_error(std::io::Error::other(
                "delegated stream cannot observe a configured usage meter; settle it buffered",
            )));
        }
        let spec = spec_from_metering(
            &plan.metering,
            SessionMeteringContext::new()
                .with_request_properties(&plan.request_properties)
                .with_optional_variant_hint(plan.variant_hint.as_deref()),
        )
        .map_err(box_error)?;
        let hints = SessionUsageHints::from_metering(
            &plan.metering,
            plan.variant_hint.as_deref(),
            &plan.request_properties,
        );
        let settlement = StablecoinSettlement::new(forward.handle.decimals());
        // A delegated signer is local to the gateway, so there is no reason to
        // release chargeable output while waiting to batch a client voucher.
        // Stablecoin precision still naturally batches sub-base-unit usage.
        let gate = SessionUsageGate::new(spec.clone(), settlement, forward.committed_base_units, 1)
            .map_err(box_error)?;
        let current = zero_observation(&spec);
        let priced_context_length = plan.request_properties.context_length;
        Ok(Self {
            forward,
            gate,
            accumulator: StreamUsageAccumulator::new(spec, hints),
            current,
            priced_context_length,
            clamp_reprice_to_authorized: false,
            authorization_exhausted: false,
            next_lifecycle_touch: Instant::now() + LIFECYCLE_TOUCH_INTERVAL,
            observer: None,
            in_flight: None,
        })
    }

    /// Use the exact forwarded request body size as the missing-input bound.
    pub fn with_request_body_bytes(mut self, bytes: u64) -> Self {
        self.accumulator.hints.request_body_bytes = Some(bytes);
        self
    }

    /// Report every committed usage increment to the handler's observer.
    pub fn with_observer(mut self, observer: Option<MeteredSettlement>) -> Self {
        self.observer = observer;
        self
    }

    pub fn supports(forward: &SessionForward) -> bool {
        let Some(plan) = forward.metered_plan() else {
            return false;
        };
        if !stream_honors_meters(&plan.metering) {
            return false;
        }
        spec_from_metering(
            &plan.metering,
            SessionMeteringContext::new()
                .with_request_properties(&plan.request_properties)
                .with_optional_variant_hint(plan.variant_hint.as_deref()),
        )
        .is_ok_and(|spec| has_stream_observable_dimension(&spec))
    }

    /// Observe one chunk and refuse it unless the stream, released so far
    /// plus this chunk, could still be paid for in full: missing provider
    /// usage settles at its upper bounds once the stream ends, so live lower
    /// bounds alone would let output out that the final charge cannot cover.
    /// A refused chunk is not counted toward the stream's usage.
    fn observe_chunk(
        &mut self,
        chunk: &[u8],
        is_sse: bool,
    ) -> Result<Option<SessionGateDecision>, BoxError> {
        let released = (
            self.accumulator.clone(),
            self.gate.clone(),
            self.current.clone(),
            self.priced_context_length,
            self.clamp_reprice_to_authorized,
        );
        let decision = self.observe_unreleased_chunk(chunk, is_sse)?;
        // A clamped reprice already settles for everything authorized.
        if decision.is_some()
            && !self.clamp_reprice_to_authorized
            && let Err(error) = self.ensure_worst_case_authorized()
        {
            (
                self.accumulator,
                self.gate,
                self.current,
                self.priced_context_length,
                self.clamp_reprice_to_authorized,
            ) = released;
            return Err(error);
        }
        Ok(decision)
    }

    fn authorized_base_units(&self) -> u64 {
        self.forward
            .committed_base_units
            .saturating_add(self.forward.available_base_units)
    }

    fn ensure_worst_case_authorized(&self) -> Result<(), BoxError> {
        let worst_case = self
            .gate
            .clone()
            .observe(self.accumulator.upper_bound_observation(), GateMode::Final)
            .map_err(box_error)?
            .target_cumulative_base_units();
        let authorized = self.authorized_base_units();
        if worst_case > authorized {
            return Err(box_error(std::io::Error::other(format!(
                "delegated session {} could owe cumulative {worst_case} without provider usage, above authorized {authorized}",
                self.forward.channel_id
            ))));
        }
        Ok(())
    }

    fn observe_unreleased_chunk(
        &mut self,
        chunk: &[u8],
        is_sse: bool,
    ) -> Result<Option<SessionGateDecision>, BoxError> {
        let output_chars_before = self.accumulator.output_chars;
        if !self.accumulator.observe_chunk(chunk, is_sse)? {
            return Ok(None);
        }
        if self.authorization_exhausted {
            return Err(box_error(std::io::Error::other(format!(
                "delegated session {} exhausted its authorization",
                self.forward.channel_id
            ))));
        }
        if let Some(context_length) = self.accumulator.observed_input_tokens()
            && self.priced_context_length != Some(context_length)
        {
            let usage_only_chunk = self.accumulator.output_chars == output_chars_before;
            self.reprice_for_context_length(context_length, usage_only_chunk)?;
        }
        self.current = self.accumulator.observation();
        self.gate
            .observe(self.current.clone(), GateMode::Streaming)
            .map(Some)
            .map_err(box_error)
    }

    fn finish(&mut self, is_sse: bool) -> MeteringResult<Option<SessionGateDecision>> {
        if self.accumulator.finish_stream(is_sse)? {
            self.current = self.accumulator.observation();
        }
        self.final_decision()
    }

    fn final_decision(&mut self) -> MeteringResult<Option<SessionGateDecision>> {
        if !self.accumulator.has_observation() || self.authorization_exhausted {
            return Ok(None);
        }
        self.gate
            .observe(self.current.clone(), GateMode::Final)
            .map(Some)
    }

    async fn settle(&mut self, decision: SessionGateDecision) -> Result<(), BoxError> {
        self.resolve_in_flight().await?;
        let clamp_reprice_to_authorized = std::mem::take(&mut self.clamp_reprice_to_authorized);
        if !decision.requires_voucher() {
            return Ok(());
        }
        let requested_target = decision.target_cumulative_base_units();
        let committed = self.gate.committed_base_units();
        let authorized = self.authorized_base_units();
        let target = if clamp_reprice_to_authorized {
            requested_target.min(authorized)
        } else {
            requested_target
        };
        let exhausted_by_reprice = clamp_reprice_to_authorized && requested_target > authorized;
        if target > authorized {
            return Err(box_error(std::io::Error::other(format!(
                "delegated session {} requires cumulative {target}, above authorized {authorized}",
                self.forward.channel_id
            ))));
        }
        let amount = target.saturating_sub(committed);
        if amount == 0 {
            self.authorization_exhausted |= exhausted_by_reprice;
            return Ok(());
        }
        let handle = Arc::clone(&self.forward.handle);
        let channel_id = self.forward.channel_id.clone();
        self.in_flight = Some(InFlightCharge {
            amount,
            commit: tokio::spawn(async move {
                handle.commit_delegated_usage(&channel_id, amount).await
            }),
        });
        self.resolve_in_flight().await?;
        self.authorization_exhausted |= exhausted_by_reprice;
        self.forward
            .handle
            .touch_after_delegated_usage(&self.forward.channel_id)
            .await
            .map_err(box_error)
    }

    /// Wait for this stream's charge in progress and account for it. The
    /// stream's own commits are additive: another stream's charges on the
    /// same channel never count toward this one.
    async fn resolve_in_flight(&mut self) -> Result<(), BoxError> {
        let Some(in_flight) = self.in_flight.as_mut() else {
            return Ok(());
        };
        // Awaiting a join handle is cancel-safe: dropping this future leaves
        // the charge running and recorded here for the next attempt.
        let joined = (&mut in_flight.commit).await;
        let amount = in_flight.amount;
        self.in_flight = None;
        joined.map_err(box_error)?.map_err(box_error)?;
        self.gate
            .record_commit(self.gate.committed_base_units().saturating_add(amount));
        if let Some(observer) = &self.observer {
            observer.record(amount, self.forward.handle.decimals());
        }
        Ok(())
    }

    fn reprice_for_context_length(
        &mut self,
        context_length: u64,
        usage_only_chunk: bool,
    ) -> Result<(), BoxError> {
        let plan = self.forward.metered_plan().ok_or_else(|| {
            box_error(std::io::Error::other(
                "delegated stream meter lost its settlement plan",
            ))
        })?;
        let mut properties = plan.request_properties;
        properties.context_length = Some(context_length);
        let spec = spec_from_metering(
            &plan.metering,
            SessionMeteringContext::new()
                .with_request_properties(&properties)
                .with_optional_variant_hint(plan.variant_hint.as_deref()),
        )
        .map_err(box_error)?;
        let hints = SessionUsageHints::from_metering(
            &plan.metering,
            plan.variant_hint.as_deref(),
            &properties,
        );
        let committed = self.gate.committed_base_units();
        let mut gate = SessionUsageGate::new(
            spec.clone(),
            StablecoinSettlement::new(self.forward.handle.decimals()),
            self.forward.committed_base_units,
            1,
        )
        .map_err(box_error)?;
        gate.record_commit(committed);
        self.accumulator.reconfigure(spec, hints);
        self.gate = gate;
        self.priced_context_length = Some(context_length);
        self.clamp_reprice_to_authorized = usage_only_chunk;
        Ok(())
    }

    fn touch_channel_if_due(&mut self) {
        let now = Instant::now();
        if now < self.next_lifecycle_touch {
            return;
        }
        self.forward
            .handle
            .touch_channel_unconfirmed(self.forward.channel_id.clone());
        self.next_lifecycle_touch = now + LIFECYCLE_TOUCH_INTERVAL;
    }

    /// Meter and durably authorize one response chunk before it is released.
    pub async fn authorize_chunk(&mut self, chunk: &[u8], is_sse: bool) -> Result<(), BoxError> {
        self.touch_channel_if_due();
        if let Some(decision) = self.observe_chunk(chunk, is_sse)? {
            self.settle(decision).await?;
        }
        Ok(())
    }

    /// Persist any final cumulative usage before the response stream closes.
    pub async fn finish_stream(&mut self, is_sse: bool) -> Result<(), BoxError> {
        if let Some(decision) = self.finish(is_sse)? {
            self.settle(decision).await?;
        }
        Ok(())
    }

    /// Persist the usage a stream that ended early implies.
    async fn abort_stream(&mut self) -> Result<(), BoxError> {
        self.resolve_in_flight().await?;
        if self.accumulator.abort_stream()? {
            self.current = self.accumulator.observation();
        }
        if let Some(decision) = self.final_decision()? {
            self.settle(decision).await?;
        }
        Ok(())
    }
}

/// Owns a delegated meter for one response body. Dropped before the stream
/// completed — client disconnect, upstream or metering error — it settles
/// the usage already released, missing-usage bounds included, so no exit
/// path skips the final charge.
struct DelegatedStreamFinalizer {
    meter: Option<DelegatedSessionStreamMeter>,
}

impl DelegatedStreamFinalizer {
    fn meter(&mut self) -> &mut DelegatedSessionStreamMeter {
        self.meter
            .as_mut()
            .expect("the meter is held until the stream completes")
    }

    async fn finish(&mut self, is_sse: bool) -> Result<(), BoxError> {
        self.meter().finish_stream(is_sse).await?;
        self.meter = None;
        Ok(())
    }
}

impl Drop for DelegatedStreamFinalizer {
    fn drop(&mut self) {
        let Some(mut meter) = self.meter.take() else {
            return;
        };
        let channel_id = meter.forward.channel_id.clone();
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn(async move {
                    if let Err(error) = meter.abort_stream().await {
                        tracing::error!(%error, %channel_id, "failed to settle an interrupted delegated stream");
                    }
                });
            }
            Err(_) => {
                tracing::error!(%channel_id, "interrupted delegated stream dropped outside a runtime; usage not settled");
            }
        }
    }
}

impl SessionStreamMeter {
    pub fn new(
        spec: SessionMeterSpec,
        hints: SessionUsageHints,
        context: SessionStreamContext,
    ) -> MeteringResult<Self> {
        let gate = SessionUsageGate::new(
            spec.clone(),
            context.settlement(),
            context.baseline_base_units(),
            context.min_voucher_delta(),
        )?;
        let current = zero_observation(&spec);
        Ok(Self {
            context,
            gate,
            accumulator: StreamUsageAccumulator::new(spec, hints),
            current,
            next_lifecycle_touch: Instant::now() + LIFECYCLE_TOUCH_INTERVAL,
        })
    }

    /// Use the exact forwarded request body size as the missing-input bound.
    pub fn with_request_body_bytes(mut self, bytes: u64) -> Self {
        self.accumulator.hints.request_body_bytes = Some(bytes);
        self
    }

    pub fn observe_chunk(
        &mut self,
        chunk: &[u8],
        is_sse: bool,
    ) -> MeteringResult<Option<SessionGateDecision>> {
        if !self.accumulator.observe_chunk(chunk, is_sse)? {
            return Ok(None);
        }

        self.current = self.accumulator.observation();
        self.gate
            .observe(self.current.clone(), GateMode::Streaming)
            .map(Some)
    }

    pub fn finish(&mut self, is_sse: bool) -> MeteringResult<Option<SessionGateDecision>> {
        if self.accumulator.finish_stream(is_sse)? {
            self.current = self.accumulator.observation();
        }
        if !self.accumulator.has_observation() {
            return Ok(None);
        }
        self.gate
            .observe(self.current.clone(), GateMode::Final)
            .map(Some)
    }

    fn record_commit(&mut self, committed_base_units: u64) {
        self.gate.record_commit(committed_base_units);
    }

    /// Meter a complete (unstreamed) response body and wait until the
    /// client's vouchers cover all of it. Its usage is only known once the
    /// whole body is parsed, so the body must not be released before this
    /// returns.
    pub async fn settle_complete_body(mut self, body: &[u8], is_sse: bool) -> Result<(), BoxError> {
        self.touch_channel_if_due();
        if let Some(decision) = self.observe_chunk(body, is_sse).map_err(box_error)? {
            settle_decision(&mut self, decision).await?;
        }
        if let Some(decision) = self.finish(is_sse).map_err(box_error)? {
            settle_decision(&mut self, decision).await?;
        }
        Ok(())
    }

    fn touch_channel_if_due(&mut self) {
        let now = Instant::now();
        if now < self.next_lifecycle_touch {
            return;
        }
        self.context.touch_channel_unconfirmed();
        self.next_lifecycle_touch = now + LIFECYCLE_TOUCH_INTERVAL;
    }
}

/// Wrap an upstream byte stream with session metering backpressure.
pub fn meter_response_stream<S>(
    stream: S,
    mut meter: SessionStreamMeter,
    is_sse: bool,
) -> impl Stream<Item = Result<Bytes, BoxError>> + Send + 'static
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
{
    async_stream::try_stream! {
        futures_util::pin_mut!(stream);
        // Bytes whose usage is below the voucher threshold are held, not
        // released, until a commit covers them: a disconnect or upstream error
        // must never leave delivered usage unpaid.
        let mut held: Vec<Bytes> = Vec::new();
        let mut held_target = 0;
        while let Some(next) = stream.next().await {
            let chunk = next.map_err(box_error)?;
            meter.touch_channel_if_due();
            let decision = meter.observe_chunk(&chunk, is_sse).map_err(box_error)?;
            held.push(chunk);
            match decision {
                Some(decision) if decision.requires_voucher() => {
                    // Do not release the bytes that crossed the voucher
                    // threshold until the cumulative commit is durable.
                    settle_decision(&mut meter, decision).await?;
                }
                Some(decision) if decision.outstanding_base_units() > 0 => {
                    held_target = decision.target_cumulative_base_units();
                }
                _ => {}
            }
            let committed = meter.context.committed_base_units();
            if committed >= held_target {
                meter.record_commit(committed);
                held_target = 0;
                for chunk in held.drain(..) {
                    yield chunk;
                }
            }
        }

        if let Some(decision) = meter.finish(is_sse).map_err(box_error)? {
            settle_decision(&mut meter, decision).await?;
        }
        for chunk in held.drain(..) {
            yield chunk;
        }
    }
}

/// Wrap a delegated-session response stream without buffering it.
///
/// Chargeable chunks remain behind the payment gate until the gateway's
/// cumulative voucher has been persisted. Dropping the returned stream early
/// settles the usage already released, then releases the meter's exclusive
/// capacity lease.
pub(crate) fn meter_delegated_response_stream<S, E>(
    stream: S,
    meter: DelegatedSessionStreamMeter,
    is_sse: bool,
) -> impl Stream<Item = Result<Bytes, BoxError>> + Send + 'static
where
    S: Stream<Item = Result<Bytes, E>> + Send + 'static,
    E: StdError + Send + Sync + 'static,
{
    let mut meter = DelegatedStreamFinalizer { meter: Some(meter) };
    async_stream::try_stream! {
        futures_util::pin_mut!(stream);
        while let Some(next) = stream.next().await {
            let chunk = next.map_err(box_error)?;
            meter.meter().authorize_chunk(&chunk, is_sse).await?;
            yield chunk;
        }

        meter.finish(is_sse).await?;
    }
}

async fn settle_decision(
    meter: &mut SessionStreamMeter,
    decision: SessionGateDecision,
) -> Result<(), BoxError> {
    if !decision.requires_voucher() {
        return Ok(());
    }

    let target = decision.target_cumulative_base_units();
    let committed = wait_for_commit(&meter.context, target).await?;
    meter.record_commit(committed);
    Ok(())
}

async fn wait_for_commit(context: &SessionStreamContext, target: u64) -> Result<u64, BoxError> {
    let wait = async {
        loop {
            let committed = context.committed_base_units();
            if committed >= target {
                return Ok(committed);
            }
            tokio::time::sleep(COMMIT_POLL_INTERVAL).await;
        }
    };

    tokio::time::timeout(COMMIT_WAIT_TIMEOUT, wait)
        .await
        .map_err(|_| {
            box_error(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!(
                    "timed out waiting for session {} voucher at cumulative {}",
                    context.session_id(),
                    target
                ),
            ))
        })?
}

#[derive(Debug, Clone)]
struct StreamUsageAccumulator {
    spec: SessionMeterSpec,
    hints: SessionUsageHints,
    sse: SseUsageDecoder,
    ndjson: NdjsonUsageDecoder,
    ndjson_utf8_tail: Vec<u8>,
    output_bytes: u64,
    output_chars: u64,
    output_words: u64,
    /// UTF-8 bytes of streamed text: an upper bound on its tokenizer units.
    output_text_bytes: u64,
    input_tokens: u64,
    output_tokens: u64,
    provider_output_tokens: bool,
    provider_input_tokens: bool,
    finished: bool,
    request_seen: bool,
    observed: bool,
}

impl StreamUsageAccumulator {
    fn new(spec: SessionMeterSpec, hints: SessionUsageHints) -> Self {
        Self {
            ndjson: NdjsonUsageDecoder::for_spec(&spec),
            spec,
            hints,
            sse: SseUsageDecoder::default(),
            ndjson_utf8_tail: Vec::new(),
            output_bytes: 0,
            output_chars: 0,
            output_words: 0,
            output_text_bytes: 0,
            input_tokens: 0,
            output_tokens: 0,
            provider_output_tokens: false,
            provider_input_tokens: false,
            finished: false,
            request_seen: false,
            observed: false,
        }
    }

    fn observe_chunk(&mut self, chunk: &[u8], is_sse: bool) -> MeteringResult<bool> {
        self.output_bytes = self.output_bytes.saturating_add(chunk.len() as u64);
        self.request_seen = true;

        let mut changed = false;
        if observes_unit(&self.spec, BillingUnit::Bytes) {
            changed = true;
        }

        if is_sse {
            changed |= self.observe_sse_chunk(chunk)?;
        } else {
            changed |= self.observe_ndjson_chunk(chunk)?;
            if observes_unit(&self.spec, BillingUnit::Characters) {
                self.output_chars = self
                    .output_chars
                    .saturating_add(count_stream_utf8_chars(&mut self.ndjson_utf8_tail, chunk)?);
                changed = true;
            }
        }

        self.observed |= changed;
        Ok(changed)
    }

    fn observe_sse_chunk(&mut self, chunk: &[u8]) -> MeteringResult<bool> {
        let events = self.sse.push_chunk(chunk)?;
        Ok(self.observe_sse_events(events))
    }

    fn observe_sse_events(&mut self, events: impl IntoIterator<Item = SseUsageEvent>) -> bool {
        let mut changed = false;
        for event in events {
            let Some(data) = event.data else {
                continue;
            };
            if data.trim() == "[DONE]" {
                continue;
            }
            let Ok(value) = serde_json::from_str::<Value>(&data) else {
                continue;
            };

            changed |= self.observe_stream_value(&value, true);
        }

        changed
    }

    /// Flush decoder state at end of stream. Returns whether usage changed.
    fn finish_stream(&mut self, is_sse: bool) -> MeteringResult<bool> {
        let changed = if is_sse {
            let event = self.sse.finish()?;
            self.observe_sse_events(event)
        } else {
            self.finish_ndjson()?
        };
        let bounded = self.settle_missing_usage()?;
        self.observed |= changed;
        Ok(changed || bounded)
    }

    /// The stream was cut short (disconnect, upstream or metering error):
    /// settle the missing-usage bounds for what it carried. Partial input is
    /// not flushed.
    fn abort_stream(&mut self) -> MeteringResult<bool> {
        if !self.request_seen {
            return Ok(false);
        }
        self.settle_missing_usage()
    }

    /// From here on, missing provider usage rates at its upper bounds.
    fn settle_missing_usage(&mut self) -> MeteringResult<bool> {
        self.finished = true;
        let mut changed = !self.provider_output_tokens && self.output_text_bytes > 0;
        if !self.provider_input_tokens
            && let Some(unit) = self.charged_input_token_unit()
        {
            let Some(body_bytes) = self.hints.request_body_bytes else {
                return Err(
                    crate::server::session_metering::SessionMeteringError::MissingRequiredObservation {
                        direction: MeterDirection::Input,
                        unit,
                    },
                );
            };
            changed |= body_bytes > self.input_tokens;
        }
        self.observed |= changed;
        Ok(changed)
    }

    fn observe_ndjson_chunk(&mut self, chunk: &[u8]) -> MeteringResult<bool> {
        let values = self.ndjson.push_chunk(chunk)?;
        Ok(values.iter().fold(false, |changed, value| {
            changed | self.observe_stream_value(value, false)
        }))
    }

    fn finish_ndjson(&mut self) -> MeteringResult<bool> {
        if !self.ndjson_utf8_tail.is_empty() {
            return Err(
                crate::server::session_metering::SessionMeteringError::InvalidStream(
                    "NDJSON stream ended inside a UTF-8 character".to_string(),
                ),
            );
        }
        let value = self.ndjson.finish()?;
        Ok(value.is_some_and(|value| self.observe_stream_value(&value, false)))
    }

    fn observe_stream_value(&mut self, value: &Value, count_text_characters: bool) -> bool {
        let mut changed = false;
        let text_chars = streamed_text_char_count(value);
        if text_chars > 0 {
            if count_text_characters {
                self.output_chars = self.output_chars.saturating_add(text_chars);
            }
            self.output_words = self
                .output_words
                .saturating_add(streamed_text_word_count(value));
            self.output_text_bytes = self
                .output_text_bytes
                .saturating_add(streamed_text_byte_count(value));
            changed = true;
        }
        if let Some(input) = provider_token_quantity(value, MeterDirection::Input) {
            self.input_tokens = self.input_tokens.max(input);
            self.provider_input_tokens = true;
            changed = true;
        }
        if let Some(output) = provider_token_quantity(value, MeterDirection::Output) {
            self.output_tokens = self.output_tokens.max(output);
            self.provider_output_tokens = true;
            changed = true;
        }
        changed
    }

    /// Input tokens to rate. Without a provider observation by end of stream,
    /// the request body size bounds the prompt from above.
    fn input_token_amount(&self) -> u64 {
        if self.finished && !self.provider_input_tokens {
            self.input_tokens
                .max(self.hints.request_body_bytes.unwrap_or(0))
        } else {
            self.input_tokens
        }
    }

    fn charged_input_token_unit(&self) -> Option<BillingUnit> {
        self.spec
            .dimensions
            .iter()
            .find(|dimension| {
                dimension.direction == MeterDirection::Input
                    && matches!(
                        dimension.unit,
                        BillingUnit::Tokens | BillingUnit::QuotaUnits
                    )
            })
            .map(|dimension| dimension.unit)
    }

    /// Output tokens to rate. Whitespace words are only a live lower bound;
    /// a stream that ends without provider usage settles the text byte count,
    /// which no tokenizer unit count can exceed.
    fn output_token_amount(&self) -> u64 {
        let floor = self.output_tokens.max(self.output_words);
        if self.finished && !self.provider_output_tokens {
            floor.max(self.output_text_bytes)
        } else {
            floor
        }
    }

    /// The observation this stream would settle if it ended now: missing
    /// provider usage at its upper bounds.
    fn upper_bound_observation(&self) -> UsageObservation {
        let mut bounded = self.clone();
        bounded.finished = true;
        bounded.observation()
    }

    fn observation(&self) -> UsageObservation {
        let mut observation = UsageObservation::new();
        for dimension in &self.spec.dimensions {
            let amount = self.dimension_amount(dimension);
            observation.set(dimension.direction, dimension.unit, amount);
        }
        observation
    }

    fn dimension_amount(&self, dimension: &SessionMeterDimension) -> u64 {
        match (dimension.direction, dimension.unit) {
            (MeterDirection::Usage, BillingUnit::Requests) => u64::from(self.request_seen),
            (MeterDirection::Output, BillingUnit::Bytes) => self.output_bytes,
            (MeterDirection::Output, BillingUnit::Characters) => self.output_chars,
            (MeterDirection::Input, BillingUnit::Tokens) => self.input_token_amount(),
            (MeterDirection::Output, BillingUnit::Tokens) => self.output_token_amount(),
            (MeterDirection::Input, BillingUnit::QuotaUnits) => {
                let input_tokens = self.input_token_amount();
                self.hints
                    .tokens_per_quota_unit(MeterDirection::Input)
                    .map(|scale| ceil_div_u64(input_tokens, scale))
                    .unwrap_or(input_tokens)
            }
            (MeterDirection::Output, BillingUnit::QuotaUnits) => {
                let output_tokens = self.output_token_amount();
                self.hints
                    .tokens_per_quota_unit(MeterDirection::Output)
                    .map(|scale| ceil_div_u64(output_tokens, scale))
                    .unwrap_or(output_tokens)
            }
            _ => 0,
        }
    }

    fn has_observation(&self) -> bool {
        self.observed
    }

    fn observed_input_tokens(&self) -> Option<u64> {
        (self.input_tokens > 0).then_some(self.input_tokens)
    }

    fn reconfigure(&mut self, spec: SessionMeterSpec, hints: SessionUsageHints) {
        let request_body_bytes = self.hints.request_body_bytes;
        self.spec = spec;
        self.hints = hints;
        self.hints.request_body_bytes = request_body_bytes.or(self.hints.request_body_bytes);
    }
}

#[derive(Debug, Clone, Default)]
struct SseUsageDecoder {
    /// Undecoded bytes: an incomplete UTF-8 sequence or a trailing `\r`.
    pending: Vec<u8>,
    buffer: String,
}

#[derive(Debug, Clone)]
struct SseUsageEvent {
    data: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct NdjsonUsageDecoder {
    buffer: Vec<u8>,
    /// Usage is read from record contents (tokens, quota units), so a
    /// malformed record would release output the meter never counted.
    records_carry_usage: bool,
}

impl NdjsonUsageDecoder {
    fn for_spec(spec: &SessionMeterSpec) -> Self {
        Self {
            buffer: Vec::new(),
            records_carry_usage: spec.dimensions.iter().any(|dimension| {
                matches!(
                    dimension.unit,
                    BillingUnit::Tokens | BillingUnit::QuotaUnits
                )
            }),
        }
    }

    fn push_chunk(&mut self, chunk: &[u8]) -> MeteringResult<Vec<Value>> {
        self.buffer.extend_from_slice(chunk);
        let mut values = Vec::new();
        while let Some(index) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let line = self.buffer[..index].to_vec();
            self.buffer.drain(..=index);
            if let Some(value) = parse_ndjson_record(&line, self.records_carry_usage)? {
                values.push(value);
            }
        }
        if self.buffer.len() > MAX_NDJSON_RECORD_BYTES {
            return Err(
                crate::server::session_metering::SessionMeteringError::InvalidStream(format!(
                    "NDJSON record exceeds {MAX_NDJSON_RECORD_BYTES} bytes"
                )),
            );
        }
        Ok(values)
    }

    fn finish(&mut self) -> MeteringResult<Option<Value>> {
        let line = std::mem::take(&mut self.buffer);
        parse_ndjson_record(&line, self.records_carry_usage)
    }
}

/// `None` for a blank line, or a malformed record when usage is measured from
/// the wire alone. When records carry usage, a malformed one is an error:
/// skipping it would release output to the client unmetered.
fn parse_ndjson_record(line: &[u8], records_carry_usage: bool) -> MeteringResult<Option<Value>> {
    let line = std::str::from_utf8(line).map_err(|error| {
        crate::server::session_metering::SessionMeteringError::InvalidStream(error.to_string())
    })?;
    let line = line.trim();
    if line.is_empty() {
        return Ok(None);
    }
    match serde_json::from_str(line) {
        Ok(value) => Ok(Some(value)),
        Err(_) if !records_carry_usage => Ok(None),
        Err(error) => Err(
            crate::server::session_metering::SessionMeteringError::InvalidStream(format!(
                "malformed NDJSON record: {error}"
            )),
        ),
    }
}

fn count_stream_utf8_chars(tail: &mut Vec<u8>, chunk: &[u8]) -> MeteringResult<u64> {
    tail.extend_from_slice(chunk);
    match std::str::from_utf8(tail) {
        Ok(text) => {
            let count = text.chars().count() as u64;
            tail.clear();
            Ok(count)
        }
        Err(error) if error.error_len().is_none() => {
            let valid = error.valid_up_to();
            let count = std::str::from_utf8(&tail[..valid])
                .expect("valid_up_to must delimit valid UTF-8")
                .chars()
                .count() as u64;
            tail.drain(..valid);
            Ok(count)
        }
        Err(error) => Err(
            crate::server::session_metering::SessionMeteringError::InvalidStream(error.to_string()),
        ),
    }
}

impl SseUsageDecoder {
    fn push_chunk(&mut self, chunk: &[u8]) -> MeteringResult<Vec<SseUsageEvent>> {
        self.pending.extend_from_slice(chunk);
        let mut valid = match std::str::from_utf8(&self.pending) {
            Ok(_) => self.pending.len(),
            Err(error) if error.error_len().is_none() => error.valid_up_to(),
            Err(error) => {
                return Err(
                    crate::server::session_metering::SessionMeteringError::InvalidStream(
                        error.to_string(),
                    ),
                );
            }
        };
        // A `\r` may be the first half of a `\r\n` split across chunks.
        if valid > 0 && self.pending[valid - 1] == b'\r' {
            valid -= 1;
        }
        self.decode(valid);

        let mut events = vec![];
        while let Some(index) = self.buffer.find("\n\n") {
            let block = self.buffer[..index].to_string();
            self.buffer.drain(..index + 2);
            events.push(parse_sse_event(&block));
        }
        Ok(events)
    }

    /// Parse a final event that was not terminated by a blank line. Released
    /// bytes are always metered, so a stream ending mid-character is invalid.
    fn finish(&mut self) -> MeteringResult<Option<SseUsageEvent>> {
        if std::str::from_utf8(&self.pending).is_err() {
            return Err(
                crate::server::session_metering::SessionMeteringError::InvalidStream(
                    "SSE stream ended inside a UTF-8 character".to_string(),
                ),
            );
        }
        self.decode(self.pending.len());
        let block = std::mem::take(&mut self.buffer);
        let block = block.trim_matches('\n');
        Ok((!block.is_empty()).then(|| parse_sse_event(block)))
    }

    fn decode(&mut self, len: usize) {
        let text = std::str::from_utf8(&self.pending[..len]).expect("validated UTF-8 prefix");
        self.buffer
            .push_str(&text.replace("\r\n", "\n").replace('\r', "\n"));
        self.pending.drain(..len);
    }
}

pub(super) fn sse_json_documents(body: &[u8]) -> Result<Vec<Value>, std::str::Utf8Error> {
    let text = std::str::from_utf8(body)?
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    // Unlike incremental decoding, a bounded buffered response can be scanned
    // once without repeatedly shifting the remaining stream in a String.
    Ok(text
        .split("\n\n")
        .filter_map(|block| parse_sse_event(block).data)
        .filter_map(|data| serde_json::from_str(&data).ok())
        .collect())
}

fn parse_sse_event(block: &str) -> SseUsageEvent {
    let mut data = vec![];
    for line in block.lines() {
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        let Some((field, value)) = line.split_once(':') else {
            continue;
        };
        if field == "data" {
            data.push(value.strip_prefix(' ').unwrap_or(value));
        }
    }
    SseUsageEvent {
        data: (!data.is_empty()).then(|| data.join("\n")),
    }
}

fn zero_observation(spec: &SessionMeterSpec) -> UsageObservation {
    let mut observation = UsageObservation::new();
    for dimension in &spec.dimensions {
        observation.set(dimension.direction, dimension.unit, 0);
    }
    observation
}

fn has_stream_observable_dimension(spec: &SessionMeterSpec) -> bool {
    spec.dimensions
        .iter()
        .any(|dimension| is_response_measured(dimension.direction, dimension.unit))
}

/// Whether the stream meter observes every configured `meter` the way
/// buffered settlement would. It reads provider token usage from canonical
/// fields only, so a header meter, a custom JSON path, or a meter on any
/// other unit must settle from the buffered response instead of being
/// silently replaced by the stream's own heuristics.
fn stream_honors_meters(metering: &Metering) -> bool {
    metering
        .dimensions
        .iter()
        .chain(metering.variants.iter().flat_map(|v| v.dimensions.iter()))
        .all(|dimension| {
            let Some(meter) = &dimension.meter else {
                return true;
            };
            if meter.source != pay_types::metering::UsageMeterSource::ResponseJson
                || dimension.unit != BillingUnit::Tokens
            {
                return false;
            }
            let canonical: &[&str] = match dimension.direction {
                MeterDirection::Input => &[
                    "/usage/prompt_tokens",
                    "/usage/input_tokens",
                    "/response/usage/input_tokens",
                    "/message/usage/input_tokens",
                    "/usageMetadata/promptTokenCount",
                ],
                MeterDirection::Output => &[
                    "/usage/completion_tokens",
                    "/usage/output_tokens",
                    "/response/usage/output_tokens",
                    "/message/usage/output_tokens",
                    "/usageMetadata/candidatesTokenCount",
                ],
                _ => &[],
            };
            meter
                .path
                .as_deref()
                .is_some_and(|path| canonical.contains(&path))
        })
}

fn observes_unit(spec: &SessionMeterSpec, unit: BillingUnit) -> bool {
    spec.dimensions
        .iter()
        .any(|dimension| dimension.unit == unit && dimension.direction == MeterDirection::Output)
}

fn select_meter_dimensions<'a>(
    metering: &'a Metering,
    variant_hint: Option<&str>,
) -> Option<&'a [MeterDimension]> {
    if !metering.variants.is_empty() {
        if let Some(variant) = resolve_variant(&metering.variants, variant_hint) {
            return Some(&variant.dimensions);
        }
        if !metering.dimensions.is_empty() {
            return Some(&metering.dimensions);
        }
        return metering
            .variants
            .first()
            .map(|variant| variant.dimensions.as_slice());
    }

    (!metering.dimensions.is_empty()).then_some(&metering.dimensions)
}

fn streamed_text_char_count(value: &Value) -> u64 {
    streamed_texts(value)
        .map(|text| text.chars().count() as u64)
        .sum()
}

fn streamed_text_byte_count(value: &Value) -> u64 {
    streamed_texts(value).map(|text| text.len() as u64).sum()
}

fn streamed_text_word_count(value: &Value) -> u64 {
    streamed_texts(value).map(count_words).sum()
}

fn streamed_texts(value: &Value) -> impl Iterator<Item = &str> {
    let gemini = value
        .get("candidates")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .flat_map(|candidate| {
            candidate
                .get("content")
                .and_then(|content| content.get("parts"))
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
        })
        .filter_map(|part| part.get("text").and_then(Value::as_str));
    let openai = value
        .get("choices")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|choice| {
            choice
                .get("delta")
                .and_then(|delta| delta.get("content"))
                .and_then(Value::as_str)
                .or_else(|| choice.get("text").and_then(Value::as_str))
        });
    let top_level_delta = value
        .get("delta")
        .and_then(|delta| {
            delta
                .as_str()
                .or_else(|| delta.get("text").and_then(Value::as_str))
        })
        .into_iter();

    gemini.chain(openai).chain(top_level_delta)
}

fn count_words(text: &str) -> u64 {
    text.split_whitespace().count() as u64
}

fn ceil_div_u64(value: u64, divisor: u64) -> u64 {
    if divisor == 0 {
        return 0;
    }
    value.saturating_add(divisor - 1) / divisor
}

fn box_error<E>(error: E) -> BoxError
where
    E: StdError + Send + Sync + 'static,
{
    Box::new(error)
}

trait OptionalVariantHint<'a> {
    fn with_optional_variant_hint(self, hint: Option<&'a str>) -> Self;
}

impl<'a> OptionalVariantHint<'a> for SessionMeteringContext<'a> {
    fn with_optional_variant_hint(self, hint: Option<&'a str>) -> Self {
        match hint {
            Some(hint) => self.with_variant_hint(hint),
            None => self,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::session::SessionHandle;
    use crate::server::metering::parse_tokens_per_quota_unit;
    use crate::server::session::test_channel_state;
    use pay_kit::mpp::blockhash::BlockhashCache;
    use pay_kit::mpp::server::session::{SessionConfig, VoucherSigner};
    use pay_kit::mpp::solana_keychain::{TransactionSigner, memory::MemorySigner};
    use pay_kit::mpp::store::{ChannelStore, MemoryChannelStore};
    use pay_types::metering::{MeterDimension, PriceTier};

    fn dimension(
        direction: MeterDirection,
        unit: BillingUnit,
        notes: Option<&str>,
    ) -> MeterDimension {
        MeterDimension {
            direction,
            unit,
            scale: 1,
            period: None,
            tiers: vec![PriceTier {
                up_to: None,
                price_usd: 0.000001,
                condition: None,
                notes: notes.map(str::to_string),
                splits: vec![],
            }],
            meter: None,
        }
    }

    fn metering(dimensions: Vec<MeterDimension>) -> Metering {
        Metering {
            dimensions,
            variants: vec![],
            sku_tiers: vec![],
            splits: vec![],
            schemes: None,
            min_usd: None,
            upto: None,
        }
    }

    fn stream_test_signer() -> Box<dyn TransactionSigner> {
        use ed25519_dalek::SigningKey;

        let signing_key = SigningKey::generate(&mut rand::thread_rng());
        let verifying_key = signing_key.verifying_key();
        let mut keypair = [0_u8; 64];
        keypair[..32].copy_from_slice(signing_key.as_bytes());
        keypair[32..].copy_from_slice(verifying_key.as_bytes());
        Box::new(MemorySigner::from_bytes(&keypair).unwrap())
    }

    #[test]
    fn parses_quota_unit_hint_from_current_yaml_notes() {
        assert_eq!(
            parse_tokens_per_quota_unit("10 input tokens per quota unit; official price"),
            Some(10)
        );
        assert_eq!(
            parse_tokens_per_quota_unit("1,000 output tokens per quota unit"),
            Some(1_000)
        );
    }

    #[test]
    fn quota_unit_hints_follow_the_selected_conditional_tier() {
        let mut quota = dimension(
            MeterDirection::Input,
            BillingUnit::QuotaUnits,
            Some("4 input tokens per quota unit"),
        );
        quota.tiers[0].condition = Some(pay_types::metering::MeterCondition::ContextLength {
            op: pay_types::metering::CompareOp::Lte,
            value: 200_000,
        });
        quota.tiers.push(PriceTier {
            up_to: None,
            price_usd: 0.000001,
            condition: Some(pay_types::metering::MeterCondition::ContextLength {
                op: pay_types::metering::CompareOp::Gt,
                value: 200_000,
            }),
            notes: Some("2 input tokens per quota unit".to_string()),
            splits: vec![],
        });
        let properties = RequestProperties {
            context_length: Some(250_000),
            ..Default::default()
        };

        let hints = SessionUsageHints::from_metering(&metering(vec![quota]), None, &properties);

        assert_eq!(hints.tokens_per_quota_unit(MeterDirection::Input), Some(2));
    }

    #[test]
    fn sse_accumulator_observes_gemini_usage_metadata() {
        let spec = SessionMeterSpec::new([
            SessionMeterDimension::required(MeterDirection::Input, BillingUnit::QuotaUnits, 1, 3),
            SessionMeterDimension::required(MeterDirection::Output, BillingUnit::QuotaUnits, 1, 5),
        ]);
        let hints = SessionUsageHints::from_metering(
            &metering(vec![
                dimension(
                    MeterDirection::Input,
                    BillingUnit::QuotaUnits,
                    Some("10 input tokens per quota unit"),
                ),
                dimension(
                    MeterDirection::Output,
                    BillingUnit::QuotaUnits,
                    Some("2 output tokens per quota unit"),
                ),
            ]),
            None,
            &RequestProperties::default(),
        );
        let mut accumulator = StreamUsageAccumulator::new(spec, hints);

        let changed = accumulator
            .observe_chunk(
                br#"data: {"usageMetadata":{"promptTokenCount":27,"candidatesTokenCount":448}}

"#,
                true,
            )
            .unwrap();
        let observation = accumulator.observation();

        assert!(changed);
        assert_eq!(
            observation.get(MeterDirection::Input, BillingUnit::QuotaUnits),
            Some(3)
        );
        assert_eq!(
            observation.get(MeterDirection::Output, BillingUnit::QuotaUnits),
            Some(224)
        );
    }

    #[test]
    fn sse_accumulator_observes_openai_compatible_usage() {
        let spec = SessionMeterSpec::new([
            SessionMeterDimension::required(MeterDirection::Input, BillingUnit::QuotaUnits, 1, 3),
            SessionMeterDimension::required(MeterDirection::Output, BillingUnit::QuotaUnits, 1, 5),
        ]);
        let hints = SessionUsageHints::from_metering(
            &metering(vec![
                dimension(
                    MeterDirection::Input,
                    BillingUnit::QuotaUnits,
                    Some("4 input tokens per quota unit"),
                ),
                dimension(
                    MeterDirection::Output,
                    BillingUnit::QuotaUnits,
                    Some("2 output tokens per quota unit"),
                ),
            ]),
            None,
            &RequestProperties::default(),
        );
        let mut accumulator = StreamUsageAccumulator::new(spec, hints);

        let changed = accumulator
            .observe_chunk(
                br#"data: {"usage":{"prompt_tokens":8,"completion_tokens":5,"total_tokens":13}}

"#,
                true,
            )
            .unwrap();
        let observation = accumulator.observation();

        assert!(changed);
        assert_eq!(
            observation.get(MeterDirection::Input, BillingUnit::QuotaUnits),
            Some(2)
        );
        assert_eq!(
            observation.get(MeterDirection::Output, BillingUnit::QuotaUnits),
            Some(3)
        );
    }

    #[test]
    fn ndjson_accumulator_observes_usage_across_chunk_boundaries() {
        let spec = SessionMeterSpec::new([
            SessionMeterDimension::required(MeterDirection::Input, BillingUnit::Tokens, 1, 1),
            SessionMeterDimension::required(MeterDirection::Output, BillingUnit::Tokens, 1, 1),
        ]);
        let mut accumulator = StreamUsageAccumulator::new(spec, SessionUsageHints::default());

        assert!(
            !accumulator
                .observe_chunk(br#"{"usage":{"prompt_tokens":8,"comple"#, false)
                .unwrap()
        );
        assert!(
            accumulator
                .observe_chunk(
                    br#"tion_tokens":5}}
"#,
                    false,
                )
                .unwrap()
        );
        let observation = accumulator.observation();

        assert_eq!(
            observation.get(MeterDirection::Input, BillingUnit::Tokens),
            Some(8)
        );
        assert_eq!(
            observation.get(MeterDirection::Output, BillingUnit::Tokens),
            Some(5)
        );
    }

    #[test]
    fn ndjson_character_meter_counts_wire_text_once() {
        let spec = SessionMeterSpec::new([SessionMeterDimension::required(
            MeterDirection::Output,
            BillingUnit::Characters,
            1,
            1,
        )]);
        let mut accumulator = StreamUsageAccumulator::new(spec, SessionUsageHints::default());
        let line = b"{\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n";

        assert!(accumulator.observe_chunk(line, false).unwrap());
        assert_eq!(
            accumulator
                .observation()
                .get(MeterDirection::Output, BillingUnit::Characters),
            Some(std::str::from_utf8(line).unwrap().chars().count() as u64)
        );
    }

    #[test]
    fn ndjson_decoder_preserves_utf8_split_between_chunks() {
        let spec = SessionMeterSpec::new([SessionMeterDimension::required(
            MeterDirection::Output,
            BillingUnit::Tokens,
            1,
            1,
        )]);
        let mut accumulator = StreamUsageAccumulator::new(spec, SessionUsageHints::default());
        let line = b"{\"note\":\"\xF0\x9F\x99\x82\",\"usage\":{\"completion_tokens\":5}}\n";
        let split = line.iter().position(|byte| *byte == 0xF0).unwrap() + 1;

        assert!(!accumulator.observe_chunk(&line[..split], false).unwrap());
        assert!(accumulator.observe_chunk(&line[split..], false).unwrap());
        assert_eq!(
            accumulator
                .observation()
                .get(MeterDirection::Output, BillingUnit::Tokens),
            Some(5)
        );
    }

    #[test]
    fn ndjson_decoder_flushes_final_record_without_newline() {
        let spec = SessionMeterSpec::new([SessionMeterDimension::required(
            MeterDirection::Output,
            BillingUnit::Tokens,
            1,
            1,
        )]);
        let mut accumulator = StreamUsageAccumulator::new(spec, SessionUsageHints::default());

        assert!(
            !accumulator
                .observe_chunk(br#"{"usage":{"completion_tokens":5}}"#, false)
                .unwrap()
        );
        assert!(accumulator.finish_ndjson().unwrap());
        assert_eq!(
            accumulator
                .observation()
                .get(MeterDirection::Output, BillingUnit::Tokens),
            Some(5)
        );
    }

    #[test]
    fn ndjson_decoder_rejects_malformed_records_and_skips_blank_lines() {
        let accumulator = || {
            StreamUsageAccumulator::new(
                SessionMeterSpec::new([SessionMeterDimension::required(
                    MeterDirection::Output,
                    BillingUnit::Tokens,
                    1,
                    1,
                )]),
                SessionUsageHints::default(),
            )
        };

        let mut blank = accumulator();
        assert!(
            blank
                .observe_chunk(b"\n  \n{\"usage\":{\"completion_tokens\":5}}\n", false)
                .unwrap()
        );

        let error = accumulator()
            .observe_chunk(b"{\"choices\":[{\"text\":\"unmetered\"}\n", false)
            .unwrap_err();
        assert!(error.to_string().contains("malformed NDJSON"), "{error}");

        // A malformed final record without a newline fails closed too.
        let mut tail = accumulator();
        tail.observe_chunk(b"{\"choices\":", false).unwrap();
        assert!(tail.finish_ndjson().is_err());

        // Wire-measured pricing counts the record's bytes either way.
        let mut bytes = StreamUsageAccumulator::new(
            SessionMeterSpec::new([SessionMeterDimension::required(
                MeterDirection::Output,
                BillingUnit::Bytes,
                1,
                1,
            )]),
            SessionUsageHints::default(),
        );
        assert!(bytes.observe_chunk(b"not json\n", false).unwrap());
        assert_eq!(
            bytes
                .observation()
                .get(MeterDirection::Output, BillingUnit::Bytes),
            Some(9)
        );
    }

    #[test]
    fn ndjson_decoder_rejects_oversized_unfinished_record() {
        let spec = SessionMeterSpec::new([SessionMeterDimension::required(
            MeterDirection::Output,
            BillingUnit::Bytes,
            1,
            1,
        )]);
        let mut accumulator = StreamUsageAccumulator::new(spec, SessionUsageHints::default());

        let error = accumulator
            .observe_chunk(&vec![b'x'; MAX_NDJSON_RECORD_BYTES + 1], false)
            .unwrap_err();
        assert!(error.to_string().contains("NDJSON record exceeds"));
    }

    #[test]
    fn sse_accumulator_observes_responses_and_anthropic_usage() {
        let spec = SessionMeterSpec::new([
            SessionMeterDimension::required(MeterDirection::Input, BillingUnit::Tokens, 1, 1),
            SessionMeterDimension::required(MeterDirection::Output, BillingUnit::Tokens, 1, 1),
        ]);

        for chunk in [
            br#"data: {"type":"response.completed","response":{"usage":{"input_tokens":8,"output_tokens":5}}}

"#
            .as_slice(),
            br#"data: {"type":"message_start","message":{"usage":{"input_tokens":8,"output_tokens":0}}}

data: {"type":"message_delta","usage":{"output_tokens":5}}

"#
            .as_slice(),
        ] {
            let mut accumulator =
                StreamUsageAccumulator::new(spec.clone(), SessionUsageHints::default());

            assert!(accumulator.observe_chunk(chunk, true).unwrap());
            assert_eq!(accumulator.observed_input_tokens(), Some(8));
            assert_eq!(
                accumulator
                    .observation()
                    .get(MeterDirection::Output, BillingUnit::Tokens),
                Some(5)
            );
        }
    }

    #[test]
    fn sse_accumulator_uses_openai_delta_words_as_live_output_floor() {
        let spec = SessionMeterSpec::new([SessionMeterDimension::required(
            MeterDirection::Output,
            BillingUnit::Tokens,
            1,
            1,
        )]);
        let mut accumulator = StreamUsageAccumulator::new(spec, SessionUsageHints::default());

        let changed = accumulator
            .observe_chunk(
                br#"data: {"choices":[{"delta":{"content":"one two three"}}]}

"#,
                true,
            )
            .unwrap();

        assert!(changed);
        assert_eq!(
            accumulator
                .observation()
                .get(MeterDirection::Output, BillingUnit::Tokens),
            Some(3)
        );
    }

    #[test]
    fn sse_accumulator_excludes_gemini_tool_prompt_tokens_from_output() {
        let spec = SessionMeterSpec::new([SessionMeterDimension::required(
            MeterDirection::Output,
            BillingUnit::QuotaUnits,
            1,
            5,
        )]);
        let hints = SessionUsageHints::from_metering(
            &metering(vec![dimension(
                MeterDirection::Output,
                BillingUnit::QuotaUnits,
                Some("1 output tokens per quota unit"),
            )]),
            None,
            &RequestProperties::default(),
        );
        let mut accumulator = StreamUsageAccumulator::new(spec, hints);

        let changed = accumulator.observe_chunk(
            br#"data: {"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":3,"thoughtsTokenCount":2,"toolUsePromptTokenCount":20,"totalTokenCount":35}}

"#,
            true,
        ).unwrap();

        assert!(changed);
        assert_eq!(
            accumulator
                .observation()
                .get(MeterDirection::Output, BillingUnit::QuotaUnits),
            Some(5)
        );
    }

    #[test]
    fn sse_accumulator_uses_text_words_as_live_output_floor() {
        let spec = SessionMeterSpec::new([SessionMeterDimension::required(
            MeterDirection::Output,
            BillingUnit::QuotaUnits,
            1,
            5,
        )]);
        let hints = SessionUsageHints::from_metering(
            &metering(vec![dimension(
                MeterDirection::Output,
                BillingUnit::QuotaUnits,
                Some("2 output tokens per quota unit"),
            )]),
            None,
            &RequestProperties::default(),
        );
        let mut accumulator = StreamUsageAccumulator::new(spec, hints);

        let changed = accumulator
            .observe_chunk(
                br#"data: {"candidates":[{"content":{"parts":[{"text":"one two three four"}]}}]}

"#,
                true,
            )
            .unwrap();

        assert!(changed);
        assert_eq!(
            accumulator
                .observation()
                .get(MeterDirection::Output, BillingUnit::QuotaUnits),
            Some(2)
        );
    }

    #[test]
    fn request_only_metering_is_not_stream_observable() {
        let spec = SessionMeterSpec::new([SessionMeterDimension::required(
            MeterDirection::Usage,
            BillingUnit::Requests,
            1,
            10_000,
        )]);

        assert!(!has_stream_observable_dimension(&spec));
    }

    #[test]
    fn gemini_first_stream_event_fits_an_authorized_quarter_dollar_session() {
        let mut input = dimension(MeterDirection::Input, BillingUnit::Tokens, None);
        input.scale = 1_000_000;
        input.tiers[0].price_usd = 0.345;
        let mut output = dimension(MeterDirection::Output, BillingUnit::Tokens, None);
        output.scale = 1_000_000;
        output.tiers[0].price_usd = 2.875;
        let metering = metering(vec![input, output]);
        let spec = spec_from_metering(&metering, SessionMeteringContext::new()).unwrap();
        let mut accumulator =
            StreamUsageAccumulator::new(spec.clone(), SessionUsageHints::default());
        assert!(accumulator
            .observe_chunk(
                b"data: {\"choices\":[{\"delta\":{\"content\":\"What\"}}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1}}\n\n",
                true,
            )
            .unwrap());
        let mut gate = SessionUsageGate::new(spec, StablecoinSettlement::usdc(), 0, 1).unwrap();
        let decision = gate
            .observe(accumulator.observation(), GateMode::Streaming)
            .unwrap();

        assert_eq!(decision.target_cumulative_base_units(), 3);
        assert!(decision.target_cumulative_base_units() <= 250_000);
    }

    fn stream_test_blockhash_cache() -> BlockhashCache {
        let cache = BlockhashCache::new();
        cache.set(
            "SURFNETxSAFEHASHxxxxxxxxxxxxxxxxxxxxx11x".to_string(),
            42,
            123,
        );
        cache
    }

    #[tokio::test]
    async fn chargeable_stream_chunk_waits_for_matching_commit() {
        let store: Arc<dyn ChannelStore> = Arc::new(MemoryChannelStore::new());
        let session = Arc::new(
            SessionMpp::new_with_channel_store(
                SessionConfig {
                    operator: solana_pubkey::Pubkey::new_unique().to_string(),
                    recipient: solana_pubkey::Pubkey::new_unique().to_string(),
                    suggested_deposit: Some(1_000),
                    currency: solana_pubkey::Pubkey::new_unique().to_string(),
                    network: "localnet".to_string(),
                    min_voucher_delta: 1,
                    ..SessionConfig::default()
                },
                "stream-test-secret",
                Arc::clone(&store),
            )
            .with_blockhash_cache(stream_test_blockhash_cache()),
        );
        let challenge = session.challenge(None).unwrap();
        let channel = solana_pubkey::Pubkey::new_unique();
        let signer = stream_test_signer();
        let authorized_signer = signer.pubkey().to_string();
        let handle = SessionHandle::new(channel, signer, challenge.clone());
        let channel_id = channel.to_string();
        store
            .put_channel(
                &channel_id,
                test_channel_state(
                    &channel_id,
                    1_000,
                    &authorized_signer,
                    "client",
                    &challenge.id,
                    solana_pubkey::Pubkey::new_unique().to_string(),
                    None,
                ),
            )
            .await
            .unwrap();
        let context = SessionStreamContext::new(session.clone(), channel_id, 0);
        let spec = SessionMeterSpec::new([SessionMeterDimension::required(
            MeterDirection::Output,
            BillingUnit::Bytes,
            1,
            1,
        )]);
        let meter = SessionStreamMeter::new(spec, SessionUsageHints::default(), context).unwrap();
        let upstream =
            futures_util::stream::iter([Ok::<_, reqwest::Error>(Bytes::from_static(b"paid"))]);
        let metered = meter_response_stream(upstream, meter, false);
        futures_util::pin_mut!(metered);

        assert!(
            tokio::time::timeout(Duration::from_millis(50), metered.next())
                .await
                .is_err(),
            "chargeable output must remain withheld while its commit is pending"
        );

        let voucher = handle.voucher_header(4).await.unwrap();
        session.process(&voucher).await.unwrap();
        let released = tokio::time::timeout(Duration::from_secs(1), metered.next())
            .await
            .expect("stream should resume after the commit")
            .expect("stream should yield the withheld chunk")
            .expect("upstream chunk should remain successful");

        assert_eq!(released, Bytes::from_static(b"paid"));
    }

    #[tokio::test]
    async fn delegated_stream_releases_each_chunk_after_its_voucher() {
        use ed25519_dalek::SigningKey;

        let signing_key = SigningKey::generate(&mut rand::thread_rng());
        let verifying_key = signing_key.verifying_key();
        let mut operator_keypair = [0_u8; 64];
        operator_keypair[..32].copy_from_slice(signing_key.as_bytes());
        operator_keypair[32..].copy_from_slice(verifying_key.as_bytes());
        let operator: Arc<dyn TransactionSigner> =
            Arc::new(MemorySigner::from_bytes(&operator_keypair).unwrap());
        let config = SessionConfig {
            operator: operator.pubkey().to_string(),
            recipient: solana_pubkey::Pubkey::new_unique().to_string(),
            suggested_deposit: Some(1_000),
            currency: solana_pubkey::Pubkey::new_unique().to_string(),
            network: "localnet".to_string(),
            min_voucher_delta: 1,
            voucher_signer: VoucherSigner::Operator,
            ..SessionConfig::default()
        };
        let store: Arc<dyn ChannelStore> = Arc::new(MemoryChannelStore::new());
        let session = Arc::new(
            SessionMpp::new_with_channel_store(
                config,
                "delegated-stream-test-secret",
                Arc::clone(&store),
            )
            .with_blockhash_cache(stream_test_blockhash_cache())
            .with_payment_channel_signer(Arc::clone(&operator)),
        );
        let challenge = session.challenge(None).unwrap();
        let channel_id = solana_pubkey::Pubkey::new_unique().to_string();
        store
            .put_channel(
                &channel_id,
                test_channel_state(
                    &channel_id,
                    1_000,
                    operator.pubkey().to_string(),
                    "operator",
                    &challenge.id,
                    solana_pubkey::Pubkey::new_unique().to_string(),
                    None,
                ),
            )
            .await
            .unwrap();
        let reservation = session
            .reserve_delegated_capacity(&channel_id, 1_000)
            .await
            .unwrap()
            .expect("session capacity should be available");
        let forward = SessionForward::delegated(
            session.clone(),
            channel_id.clone(),
            0,
            crate::server::metering::UptoSettlementPlan {
                metering: metering(vec![MeterDimension {
                    direction: MeterDirection::Output,
                    unit: BillingUnit::Tokens,
                    scale: 1,
                    period: None,
                    tiers: vec![
                        PriceTier {
                            up_to: None,
                            price_usd: 0.000001,
                            condition: Some(pay_types::metering::MeterCondition::ContextLength {
                                op: pay_types::metering::CompareOp::Lte,
                                value: 2,
                            }),
                            notes: None,
                            splits: vec![],
                        },
                        PriceTier {
                            up_to: None,
                            price_usd: 0.000005,
                            condition: Some(pay_types::metering::MeterCondition::ContextLength {
                                op: pay_types::metering::CompareOp::Gt,
                                value: 2,
                            }),
                            notes: None,
                            splits: vec![],
                        },
                    ],
                    meter: None,
                }]),
                variant_hint: None,
                request_properties: RequestProperties::default(),
                ceiling_usd: 0.001,
                inferred_usage: None,
            },
            (10, 1_000),
            solana_pubkey::Pubkey::new_unique().to_string(),
            reservation,
        );
        let meter = DelegatedSessionStreamMeter::from_forward(forward).unwrap();
        let release_second = Arc::new(tokio::sync::Notify::new());
        let upstream_release = Arc::clone(&release_second);
        let upstream = async_stream::stream! {
            yield Ok::<_, std::io::Error>(Bytes::from_static(
                b"data: {\"choices\":[{\"delta\":{\"content\":\"a b c\"}}]}\n\n"
            ));
            upstream_release.notified().await;
            yield Ok::<_, std::io::Error>(Bytes::from_static(
                b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":3}}\n\n"
            ));
        };
        let metered = meter_delegated_response_stream(upstream, meter, true);
        futures_util::pin_mut!(metered);

        let first = tokio::time::timeout(Duration::from_secs(1), metered.next())
            .await
            .expect("first chunk must not wait for upstream EOF")
            .expect("stream should yield its first chunk")
            .expect("first chunk should remain successful");
        assert_eq!(
            first,
            Bytes::from_static(b"data: {\"choices\":[{\"delta\":{\"content\":\"a b c\"}}]}\n\n")
        );
        assert_eq!(session.committed_watermark(&channel_id), Some(3));

        assert!(
            tokio::time::timeout(Duration::from_millis(50), metered.next())
                .await
                .is_err(),
            "second chunk should still be waiting on the upstream"
        );
        release_second.notify_one();
        let second = tokio::time::timeout(Duration::from_secs(1), metered.next())
            .await
            .expect("second chunk should resume")
            .expect("stream should yield its second chunk")
            .expect("second chunk should remain successful");
        assert_eq!(
            second,
            Bytes::from_static(
                b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":3}}\n\n"
            )
        );
        assert_eq!(
            session.committed_watermark(&channel_id),
            Some(10),
            "retroactive repricing must collect the remaining authorization without failing the stream"
        );
        assert!(
            metered.next().await.is_none(),
            "a clamped usage-only reprice must complete cleanly"
        );
    }

    /// A delegated (operator-signed) stream meter over a funded test channel.
    async fn delegated_test_meter(
        dimensions: Vec<MeterDimension>,
        label: &str,
    ) -> (Arc<SessionMpp>, String, DelegatedSessionStreamMeter) {
        delegated_test_meter_on(dimensions, label, Arc::new(MemoryChannelStore::new())).await
    }

    async fn delegated_test_meter_on(
        dimensions: Vec<MeterDimension>,
        label: &str,
        store: Arc<dyn ChannelStore>,
    ) -> (Arc<SessionMpp>, String, DelegatedSessionStreamMeter) {
        let channel = delegated_channel(label, &store).await;
        let meter = delegated_meter(&channel.session, &channel.channel_id, dimensions).await;
        (channel.session, channel.channel_id, meter)
    }

    struct DelegatedChannel {
        config: SessionConfig,
        operator: Arc<dyn TransactionSigner>,
        session: Arc<SessionMpp>,
        channel_id: String,
    }

    /// A funded operator-signed channel in `store`, served by one replica.
    async fn delegated_channel(label: &str, store: &Arc<dyn ChannelStore>) -> DelegatedChannel {
        use ed25519_dalek::SigningKey;

        let signing_key = SigningKey::generate(&mut rand::thread_rng());
        let mut operator_keypair = [0_u8; 64];
        operator_keypair[..32].copy_from_slice(signing_key.as_bytes());
        operator_keypair[32..].copy_from_slice(signing_key.verifying_key().as_bytes());
        let operator: Arc<dyn TransactionSigner> =
            Arc::new(MemorySigner::from_bytes(&operator_keypair).unwrap());
        let config = SessionConfig {
            operator: operator.pubkey().to_string(),
            recipient: solana_pubkey::Pubkey::new_unique().to_string(),
            suggested_deposit: Some(1_000_000),
            currency: solana_pubkey::Pubkey::new_unique().to_string(),
            network: "localnet".to_string(),
            min_voucher_delta: 1,
            voucher_signer: VoucherSigner::Operator,
            ..SessionConfig::default()
        };
        let session = delegated_replica(&config, &operator, label, store);
        let challenge = session.challenge(None).unwrap();
        let channel_id = solana_pubkey::Pubkey::new_unique().to_string();
        store
            .put_channel(
                &channel_id,
                test_channel_state(
                    &channel_id,
                    1_000_000,
                    operator.pubkey().to_string(),
                    "operator",
                    &challenge.id,
                    solana_pubkey::Pubkey::new_unique().to_string(),
                    None,
                ),
            )
            .await
            .unwrap();
        DelegatedChannel {
            config,
            operator,
            session,
            channel_id,
        }
    }

    /// One gateway replica over `store`.
    fn delegated_replica(
        config: &SessionConfig,
        operator: &Arc<dyn TransactionSigner>,
        label: &str,
        store: &Arc<dyn ChannelStore>,
    ) -> Arc<SessionMpp> {
        Arc::new(
            SessionMpp::new_with_channel_store(config.clone(), label, Arc::clone(store))
                .with_blockhash_cache(stream_test_blockhash_cache())
                .with_payment_channel_signer(Arc::clone(operator)),
        )
    }

    /// A stream admitted on `session` at the channel's current watermark.
    async fn delegated_meter(
        session: &Arc<SessionMpp>,
        channel_id: &str,
        dimensions: Vec<MeterDimension>,
    ) -> DelegatedSessionStreamMeter {
        delegated_meter_with_capacity(session, channel_id, dimensions, 100_000).await
    }

    /// A stream admitted with `capacity` base units authorized for it.
    async fn delegated_meter_with_capacity(
        session: &Arc<SessionMpp>,
        channel_id: &str,
        dimensions: Vec<MeterDimension>,
        capacity: u64,
    ) -> DelegatedSessionStreamMeter {
        DelegatedSessionStreamMeter::from_forward(
            delegated_forward_with_capacity(session, channel_id, dimensions, capacity).await,
        )
        .unwrap()
    }

    async fn delegated_forward(
        session: &Arc<SessionMpp>,
        channel_id: &str,
        dimensions: Vec<MeterDimension>,
    ) -> SessionForward {
        delegated_forward_with_capacity(session, channel_id, dimensions, 100_000).await
    }

    async fn delegated_forward_with_capacity(
        session: &Arc<SessionMpp>,
        channel_id: &str,
        dimensions: Vec<MeterDimension>,
        capacity: u64,
    ) -> SessionForward {
        let reservation = session
            .reserve_delegated_capacity(channel_id, capacity)
            .await
            .unwrap()
            .expect("session capacity should be available");
        SessionForward::delegated(
            session.clone(),
            channel_id.to_string(),
            0,
            crate::server::metering::UptoSettlementPlan {
                metering: metering(dimensions),
                variant_hint: None,
                request_properties: RequestProperties::default(),
                ceiling_usd: capacity as f64 / 1_000_000.0,
                inferred_usage: None,
            },
            (capacity, 1_000_000),
            solana_pubkey::Pubkey::new_unique().to_string(),
            reservation,
        )
    }

    #[tokio::test]
    async fn a_meter_the_stream_cannot_observe_settles_from_the_buffered_response() {
        let metered = |meter| MeterDimension {
            direction: MeterDirection::Output,
            unit: BillingUnit::Bytes,
            scale: 1,
            period: None,
            tiers: vec![PriceTier {
                up_to: None,
                price_usd: 0.000001,
                condition: None,
                notes: None,
                splits: vec![],
            }],
            meter: Some(meter),
        };
        let header_meter = pay_types::metering::UsageMeter {
            source: pay_types::metering::UsageMeterSource::ResponseHeader,
            path: None,
            header: Some("x-billable-bytes".into()),
        };
        let store: Arc<dyn ChannelStore> = Arc::new(MemoryChannelStore::new());
        let channel = delegated_channel("header-meter", &store).await;
        let forward = delegated_forward(
            &channel.session,
            &channel.channel_id,
            vec![metered(header_meter)],
        )
        .await;
        assert!(!DelegatedSessionStreamMeter::supports(&forward));

        let body = "data: {\"payload\":\"ok\"}\n\n";
        let response = axum::response::Response::builder()
            .status(http::StatusCode::OK)
            .header(http::header::CONTENT_TYPE, "text/event-stream")
            .header("x-billable-bytes", "5000")
            .body(axum::body::Body::from(body))
            .unwrap();
        let response =
            crate::server::payment::settle_axum_delegated_response(forward, response).await;
        assert_eq!(response.status(), http::StatusCode::OK);
        let served = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(served, body.as_bytes());
        // The configured header quantity is charged, not the wire bytes.
        assert_eq!(
            channel.session.committed_watermark(&channel.channel_id),
            Some(5000)
        );

        // Canonical provider token paths stay on the streaming path.
        let canonical = delegated_forward(
            &channel.session,
            &channel.channel_id,
            vec![token_dimension(
                MeterDirection::Output,
                "/usage/completion_tokens",
            )],
        )
        .await;
        assert!(DelegatedSessionStreamMeter::supports(&canonical));
        drop(canonical);
        let custom = delegated_forward(
            &channel.session,
            &channel.channel_id,
            vec![token_dimension(MeterDirection::Output, "/billing/tokens")],
        )
        .await;
        assert!(!DelegatedSessionStreamMeter::supports(&custom));
    }

    fn token_dimension(direction: MeterDirection, path: &str) -> MeterDimension {
        MeterDimension {
            direction,
            unit: BillingUnit::Tokens,
            scale: 1,
            period: None,
            tiers: vec![PriceTier {
                up_to: None,
                price_usd: 0.000001,
                condition: None,
                notes: None,
                splits: vec![],
            }],
            meter: Some(pay_types::metering::UsageMeter {
                source: pay_types::metering::UsageMeterSource::ResponseJson,
                path: Some(path.to_string()),
                header: None,
            }),
        }
    }

    async fn drain_delegated_bytes(
        chunks: Vec<Bytes>,
        meter: DelegatedSessionStreamMeter,
    ) -> Result<Vec<u8>, BoxError> {
        drain_delegated(
            chunks.into_iter().map(|chunk| chunk.to_vec()).collect(),
            meter,
        )
        .await
    }

    async fn drain_delegated(
        chunks: Vec<Vec<u8>>,
        meter: DelegatedSessionStreamMeter,
    ) -> Result<Vec<u8>, BoxError> {
        let upstream = futures_util::stream::iter(
            chunks
                .into_iter()
                .map(|chunk| Ok::<_, std::io::Error>(Bytes::from(chunk))),
        );
        let metered = meter_delegated_response_stream(upstream, meter, true);
        futures_util::pin_mut!(metered);
        let mut served = Vec::new();
        while let Some(chunk) = metered.next().await {
            served.extend_from_slice(&chunk?);
        }
        Ok(served)
    }

    #[tokio::test]
    async fn delegated_sse_meters_events_with_utf8_split_across_chunks() {
        let event = b"data: {\"choices\":[{\"delta\":{\"content\":\"\xC3\xA9\"}}],\"usage\":{\"completion_tokens\":5}}\n\n";
        let split = event.iter().position(|byte| *byte == 0xC3).unwrap() + 1;
        let (session, channel_id, meter) = delegated_test_meter(
            vec![token_dimension(
                MeterDirection::Output,
                "/usage/completion_tokens",
            )],
            "sse-utf8-split",
        )
        .await;

        let served = drain_delegated(
            vec![event[..split].to_vec(), event[split..].to_vec()],
            meter,
        )
        .await
        .unwrap();

        assert_eq!(served, event);
        assert_eq!(session.committed_watermark(&channel_id), Some(5));
    }

    #[tokio::test]
    async fn delegated_sse_meters_unsplit_utf8_event() {
        let event = "data: {\"choices\":[{\"delta\":{\"content\":\"é\"}}],\"usage\":{\"completion_tokens\":5}}\n\n";
        let (session, channel_id, meter) = delegated_test_meter(
            vec![token_dimension(
                MeterDirection::Output,
                "/usage/completion_tokens",
            )],
            "sse-utf8-whole",
        )
        .await;

        drain_delegated(vec![event.as_bytes().to_vec()], meter)
            .await
            .unwrap();

        assert_eq!(session.committed_watermark(&channel_id), Some(5));
    }

    #[test]
    fn sse_decoder_handles_every_utf8_and_crlf_split_point() {
        let stream = "data: {\"t\":\"a€😀\"}\r\n\r\ndata: {\"t\":\"z\"}\r\n\r\n".as_bytes();
        for split in 0..=stream.len() {
            let mut decoder = SseUsageDecoder::default();
            let mut events = decoder.push_chunk(&stream[..split]).unwrap();
            events.extend(decoder.push_chunk(&stream[split..]).unwrap());
            assert!(decoder.finish().unwrap().is_none(), "split {split}");
            let data: Vec<_> = events.into_iter().filter_map(|event| event.data).collect();
            assert_eq!(
                data,
                vec!["{\"t\":\"a€😀\"}", "{\"t\":\"z\"}"],
                "split {split}"
            );
        }
    }

    #[test]
    fn sse_decoder_rejects_malformed_utf8() {
        let mut decoder = SseUsageDecoder::default();
        assert!(decoder.push_chunk(b"data: \xFF\n\n").is_err());
    }

    #[test]
    fn sse_decoder_rejects_stream_ending_inside_a_character() {
        let mut decoder = SseUsageDecoder::default();
        assert!(
            decoder
                .push_chunk(b"data: {\"t\":\"\xE2\x82")
                .unwrap()
                .is_empty()
        );
        assert!(decoder.finish().is_err());
    }

    #[test]
    fn sse_decoder_flushes_final_event_without_blank_line() {
        let mut decoder = SseUsageDecoder::default();
        assert!(
            decoder
                .push_chunk(b"data: {\"usage\":{\"completion_tokens\":7}}\n")
                .unwrap()
                .is_empty()
        );
        let event = decoder.finish().unwrap().expect("final event");
        assert_eq!(
            event.data.as_deref(),
            Some("{\"usage\":{\"completion_tokens\":7}}")
        );
    }

    #[tokio::test]
    async fn delegated_sse_meters_final_event_without_blank_line() {
        let (session, channel_id, meter) = delegated_test_meter(
            vec![token_dimension(
                MeterDirection::Output,
                "/usage/completion_tokens",
            )],
            "sse-final-event",
        )
        .await;

        drain_delegated(
            vec![b"data: {\"choices\":[],\"usage\":{\"completion_tokens\":7}}".to_vec()],
            meter,
        )
        .await
        .unwrap();

        assert_eq!(session.committed_watermark(&channel_id), Some(7));
    }

    fn sse_text_event(text: &str) -> Vec<u8> {
        let chunk = serde_json::json!({"choices": [{"delta": {"content": text}}]});
        format!("data: {chunk}\n\n").into_bytes()
    }

    #[tokio::test]
    async fn delegated_token_stream_without_provider_usage_settles_text_upper_bound() {
        let payload = "!?".repeat(256);
        assert_eq!(count_words(&payload), 1);
        let (session, channel_id, meter) = delegated_test_meter(
            vec![token_dimension(
                MeterDirection::Output,
                "/usage/completion_tokens",
            )],
            "missing-usage-upper-bound",
        )
        .await;

        drain_delegated(
            vec![sse_text_event(&payload), b"data: [DONE]\n\n".to_vec()],
            meter,
        )
        .await
        .unwrap();

        assert_eq!(
            session.committed_watermark(&channel_id),
            Some(payload.len() as u64),
            "a word count must never be the final token settlement"
        );
    }

    #[tokio::test]
    async fn delegated_token_stream_settles_provider_usage_when_reported() {
        let payload = "!?".repeat(256);
        let (session, channel_id, meter) = delegated_test_meter(
            vec![token_dimension(
                MeterDirection::Output,
                "/usage/completion_tokens",
            )],
            "provider-usage-authoritative",
        )
        .await;

        drain_delegated(
            vec![
                sse_text_event(&payload),
                b"data: {\"choices\":[],\"usage\":{\"completion_tokens\":300}}\n\n".to_vec(),
                b"data: [DONE]\n\n".to_vec(),
            ],
            meter,
        )
        .await
        .unwrap();

        assert_eq!(session.committed_watermark(&channel_id), Some(300));
    }

    /// Streams one text event under `capacity`, returning what reached the
    /// client and how the stream ended.
    async fn capacity_bound_stream(
        label: &str,
        capacity: u64,
    ) -> (Arc<SessionMpp>, String, Vec<u8>, Result<(), BoxError>) {
        let store: Arc<dyn ChannelStore> = Arc::new(MemoryChannelStore::new());
        let channel = delegated_channel(label, &store).await;
        let meter = delegated_meter_with_capacity(
            &channel.session,
            &channel.channel_id,
            vec![token_dimension(
                MeterDirection::Output,
                "/usage/completion_tokens",
            )],
            capacity,
        )
        .await;
        // One whitespace word, 512 UTF-8 bytes: the live lower bound is far
        // below what the stream settles if the provider never reports usage.
        let upstream = futures_util::stream::iter([
            Ok::<_, std::io::Error>(Bytes::from(sse_text_event(&"!?".repeat(256)))),
            Ok(Bytes::from_static(b"data: [DONE]\n\n")),
        ]);
        let metered = meter_delegated_response_stream(upstream, meter, true);
        futures_util::pin_mut!(metered);
        let mut served = Vec::new();
        let mut outcome = Ok(());
        while let Some(chunk) = metered.next().await {
            match chunk {
                Ok(chunk) => served.extend_from_slice(&chunk),
                Err(error) => {
                    outcome = Err(error);
                    break;
                }
            }
        }
        (channel.session, channel.channel_id, served, outcome)
    }

    #[tokio::test]
    async fn delegated_stream_withholds_output_its_authorization_cannot_cover() {
        let (session, channel_id, served, outcome) =
            capacity_bound_stream("bound-above-capacity", 100).await;

        assert!(served.is_empty(), "the text must not be released");
        let error = outcome.expect_err("the stream must fail");
        assert!(
            error.to_string().contains("above authorized 100"),
            "unexpected error: {error}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            session.committed_watermark(&channel_id).unwrap_or(0),
            0,
            "nothing was released, so nothing is charged"
        );
        assert!(
            session
                .reserve_delegated_capacity(&channel_id, 1)
                .await
                .unwrap()
                .is_some(),
            "the refused stream releases its capacity"
        );
    }

    #[tokio::test]
    async fn delegated_stream_releases_output_its_authorization_covers() {
        let (session, channel_id, served, outcome) =
            capacity_bound_stream("bound-within-capacity", 512).await;

        outcome.unwrap();
        assert!(served.ends_with(b"data: [DONE]\n\n"));
        assert_eq!(session.committed_watermark(&channel_id), Some(512));
    }

    async fn interrupted_meter(
        label: &str,
    ) -> (Arc<SessionMpp>, String, DelegatedSessionStreamMeter) {
        let (session, channel_id, meter) = delegated_test_meter(
            vec![
                token_dimension(MeterDirection::Input, "/usage/prompt_tokens"),
                token_dimension(MeterDirection::Output, "/usage/completion_tokens"),
            ],
            label,
        )
        .await;
        (session, channel_id, meter.with_request_body_bytes(40))
    }

    async fn settled_watermark(
        session: &SessionMpp,
        channel_id: &str,
        expected: u64,
    ) -> Option<u64> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let watermark = session.committed_watermark(channel_id);
            if watermark == Some(expected) || Instant::now() > deadline {
                return watermark;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn an_upstream_error_still_settles_the_missing_usage_bounds() {
        let payload = "!?".repeat(64);
        let (session, channel_id, meter) = interrupted_meter("upstream-error-bounds").await;
        let upstream = futures_util::stream::iter(vec![
            Ok(Bytes::from(sse_text_event(&payload))),
            Err(std::io::Error::other("upstream reset")),
        ]);
        let mut metered = Box::pin(meter_delegated_response_stream(upstream, meter, true));
        assert!(metered.next().await.unwrap().is_ok(), "text is released");
        assert!(metered.next().await.unwrap().is_err());
        drop(metered);

        // Input from the request body bound, output from the text bytes.
        let expected = 40 + payload.len() as u64;
        assert_eq!(
            settled_watermark(&session, &channel_id, expected).await,
            Some(expected)
        );
    }

    #[tokio::test]
    async fn a_client_disconnect_still_settles_the_missing_usage_bounds() {
        let payload = "!?".repeat(64);
        let (session, channel_id, meter) = interrupted_meter("disconnect-bounds").await;
        let upstream = futures_util::stream::iter(vec![Ok::<_, std::io::Error>(Bytes::from(
            sse_text_event(&payload),
        ))])
        .chain(futures_util::stream::pending());
        let mut metered = Box::pin(meter_delegated_response_stream(upstream, meter, true));
        assert!(metered.next().await.unwrap().is_ok(), "text is released");
        // The buyer disconnects before the stream completes.
        drop(metered);

        let expected = 40 + payload.len() as u64;
        assert_eq!(
            settled_watermark(&session, &channel_id, expected).await,
            Some(expected)
        );
    }

    /// A store that stalls once the watermark reaches `stall_at`: its next
    /// read never answers (cancelling an attempt in the lifecycle touch after
    /// its voucher saved), or with `slow_write`, the write that saved it is
    /// slow to return (cancelling the attempt inside the commit itself).
    /// Once armed with [`Self::hold_reads_in_lockstep`], its next reads also
    /// wait for each other, so two replicas observe the same watermark.
    struct StallingStore {
        inner: MemoryChannelStore,
        stall_at: u64,
        slow_write: bool,
        stalled: std::sync::atomic::AtomicBool,
        lockstep: std::sync::Mutex<Option<(Arc<tokio::sync::Barrier>, usize)>>,
    }

    impl StallingStore {
        fn new(stall_at: u64, slow_write: bool) -> Arc<Self> {
            Arc::new(Self {
                inner: MemoryChannelStore::new(),
                stall_at,
                slow_write,
                stalled: std::sync::atomic::AtomicBool::new(false),
                lockstep: std::sync::Mutex::new(None),
            })
        }

        /// Make the next `readers` reads wait until all of them have arrived.
        fn hold_reads_in_lockstep(&self, readers: usize) {
            *self.lockstep.lock().unwrap() =
                Some((Arc::new(tokio::sync::Barrier::new(readers)), readers));
        }

        fn lockstep_barrier(&self) -> Option<Arc<tokio::sync::Barrier>> {
            let mut lockstep = self.lockstep.lock().unwrap();
            let (barrier, remaining) = lockstep.as_mut()?;
            let barrier = Arc::clone(barrier);
            *remaining -= 1;
            if *remaining == 0 {
                *lockstep = None;
            }
            Some(barrier)
        }

        async fn after_write(&self, channel_id: &str) {
            if !self.slow_write {
                return;
            }
            let saved = self
                .inner
                .get_channel(channel_id)
                .await
                .ok()
                .flatten()
                .is_some_and(|state| state.cumulative >= self.stall_at);
            if saved && !self.stalled.swap(true, std::sync::atomic::Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(400)).await;
            }
        }

        fn stalled(&self) -> bool {
            self.stalled.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    type StoreFuture<'a, T> = std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<T, pay_kit::mpp::store::StoreError>>
                + Send
                + 'a,
        >,
    >;

    impl ChannelStore for StallingStore {
        fn list_channels(&self) -> StoreFuture<'_, Vec<pay_kit::mpp::store::ChannelState>> {
            self.inner.list_channels()
        }

        fn get_channel(
            &self,
            channel_id: &str,
        ) -> StoreFuture<'_, Option<pay_kit::mpp::store::ChannelState>> {
            let channel_id = channel_id.to_string();
            let lockstep = self.lockstep_barrier();
            Box::pin(async move {
                let state = self.inner.get_channel(&channel_id).await?;
                // Return the snapshot only once every lockstep reader holds one.
                if let Some(barrier) = lockstep {
                    barrier.wait().await;
                }
                if !self.slow_write
                    && state
                        .as_ref()
                        .is_some_and(|state| state.cumulative >= self.stall_at)
                    && !self.stalled.swap(true, std::sync::atomic::Ordering::SeqCst)
                {
                    std::future::pending::<()>().await;
                }
                Ok(state)
            })
        }

        fn put_channel(
            &self,
            channel_id: &str,
            state: pay_kit::mpp::store::ChannelState,
        ) -> StoreFuture<'_, ()> {
            self.inner.put_channel(channel_id, state)
        }

        fn delete_channel(&self, channel_id: &str) -> StoreFuture<'_, ()> {
            self.inner.delete_channel(channel_id)
        }

        fn update_channel(
            &self,
            channel_id: &str,
            updater: Box<
                dyn FnOnce(
                        Option<pay_kit::mpp::store::ChannelState>,
                    ) -> Result<
                        pay_kit::mpp::store::ChannelState,
                        pay_kit::mpp::store::StoreError,
                    > + Send,
            >,
        ) -> StoreFuture<'_, pay_kit::mpp::store::ChannelState> {
            let channel_id = channel_id.to_string();
            Box::pin(async move {
                let state = self.inner.update_channel(&channel_id, updater).await?;
                self.after_write(&channel_id).await;
                Ok(state)
            })
        }

        fn read_channel(
            &self,
            channel_id: &str,
            reader: Box<
                dyn FnOnce(
                        Option<&pay_kit::mpp::store::ChannelState>,
                    ) -> Result<(), pay_kit::mpp::store::StoreError>
                    + Send,
            >,
        ) -> StoreFuture<'_, ()> {
            self.inner.read_channel(channel_id, reader)
        }

        fn mutate_channel(
            &self,
            channel_id: &str,
            seed: Option<pay_kit::mpp::store::ChannelState>,
            mutator: Box<
                dyn FnOnce(
                        &mut pay_kit::mpp::store::ChannelState,
                    ) -> Result<(), pay_kit::mpp::store::StoreError>
                    + Send,
            >,
        ) -> StoreFuture<'_, ()> {
            let channel_id = channel_id.to_string();
            Box::pin(async move {
                self.inner
                    .mutate_channel(&channel_id, seed, mutator)
                    .await?;
                self.after_write(&channel_id).await;
                Ok(())
            })
        }

        fn touch_channel_lifecycle(
            &self,
            channel_id: &str,
            lifecycle: pay_kit::mpp::store::ChannelLifecycle,
        ) -> StoreFuture<'_, pay_kit::mpp::store::ChannelState> {
            self.inner.touch_channel_lifecycle(channel_id, lifecycle)
        }

        fn advance_cumulative(
            &self,
            channel_id: &str,
            expected: u64,
            new: u64,
        ) -> StoreFuture<'_, bool> {
            self.inner.advance_cumulative(channel_id, expected, new)
        }

        fn update_deposit(&self, channel_id: &str, new_deposit: u64) -> StoreFuture<'_, ()> {
            self.inner.update_deposit(channel_id, new_deposit)
        }

        fn mark_sealed(&self, channel_id: &str) -> StoreFuture<'_, ()> {
            self.inner.mark_sealed(channel_id)
        }

        fn mark_finalized(&self, channel_id: &str) -> StoreFuture<'_, ()> {
            self.inner.mark_finalized(channel_id)
        }
    }

    #[tokio::test]
    async fn a_cancelled_settlement_is_never_charged_twice() {
        let store = StallingStore::new(5, false);
        let (session, channel_id, meter) = delegated_test_meter_on(
            vec![token_dimension(
                MeterDirection::Output,
                "/usage/completion_tokens",
            )],
            "cancelled-settlement",
            store.clone(),
        )
        .await;
        let upstream = futures_util::stream::iter(vec![Ok::<_, std::io::Error>(usage_event(5))])
            .chain(futures_util::stream::pending());
        let mut metered = Box::pin(meter_delegated_response_stream(upstream, meter, true));
        // The voucher for 5 is saved, then the lifecycle touch never returns.
        assert!(
            tokio::time::timeout(Duration::from_millis(200), metered.next())
                .await
                .is_err()
        );
        assert!(store.stalled());
        // The buyer disconnects mid-settlement; the finalizer retries.
        drop(metered);

        tokio::time::sleep(Duration::from_millis(200)).await;
        let state = store.inner.get_channel(&channel_id).await.unwrap().unwrap();
        assert_eq!(state.cumulative, 5, "the usage is charged exactly once");
        assert_eq!(session.committed_watermark(&channel_id), Some(5));
    }

    fn usage_event(completion_tokens: u64) -> Bytes {
        Bytes::from(format!(
            "data: {{\"choices\":[],\"usage\":{{\"completion_tokens\":{completion_tokens}}}}}\n\n"
        ))
    }

    #[tokio::test]
    async fn a_charge_cancelled_inside_its_commit_is_never_charged_twice() {
        let store = StallingStore::new(5, true);
        let (session, channel_id, meter) = delegated_test_meter_on(
            vec![token_dimension(
                MeterDirection::Output,
                "/usage/completion_tokens",
            )],
            "cancelled-commit",
            store.clone(),
        )
        .await;
        let upstream = futures_util::stream::iter(vec![Ok::<_, std::io::Error>(usage_event(5))])
            .chain(futures_util::stream::pending());
        let mut metered = Box::pin(meter_delegated_response_stream(upstream, meter, true));
        // The voucher for 5 is saved; the write reporting it is still out.
        assert!(
            tokio::time::timeout(Duration::from_millis(200), metered.next())
                .await
                .is_err()
        );
        assert!(store.stalled());
        drop(metered);

        tokio::time::sleep(Duration::from_millis(600)).await;
        let state = store.inner.get_channel(&channel_id).await.unwrap().unwrap();
        assert_eq!(state.cumulative, 5, "the usage is charged exactly once");
        assert_eq!(session.committed_watermark(&channel_id), Some(5));
    }

    #[tokio::test]
    async fn streams_on_two_replicas_each_pay_their_own_usage() {
        let store: Arc<dyn ChannelStore> = Arc::new(MemoryChannelStore::new());
        let dimensions = || {
            vec![token_dimension(
                MeterDirection::Output,
                "/usage/completion_tokens",
            )]
        };
        let channel = delegated_channel("two-replicas", &store).await;
        let channel_id = channel.channel_id.clone();
        let first_meter = delegated_meter(&channel.session, &channel_id, dimensions()).await;
        // A second gateway over the same store admits a stream on the same
        // channel at the same watermark.
        let second = delegated_replica(&channel.config, &channel.operator, "two-replicas", &store);
        let second_meter = delegated_meter(&second, &channel_id, dimensions()).await;

        drain_delegated_bytes(vec![usage_event(5)], first_meter)
            .await
            .unwrap();
        drain_delegated_bytes(vec![usage_event(5), usage_event(7)], second_meter)
            .await
            .unwrap();

        let state = store.get_channel(&channel_id).await.unwrap().unwrap();
        assert_eq!(state.cumulative, 12, "each stream pays its own usage");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn replicas_racing_one_watermark_each_pay_their_own_request() {
        let store = StallingStore::new(u64::MAX, false);
        let shared: Arc<dyn ChannelStore> = store.clone();
        let channel = delegated_channel("racing-replicas", &shared).await;
        let channel_id = channel.channel_id.clone();
        let second = delegated_replica(
            &channel.config,
            &channel.operator,
            "racing-replicas",
            &shared,
        );
        // Both replicas read the channel at cumulative 0 and sign the same
        // deterministic voucher for 25; only one of them may land it.
        store.hold_reads_in_lockstep(2);
        let (first, second) = tokio::join!(
            channel.session.authorize_delegated_usage(&channel_id, 25),
            second.authorize_delegated_usage(&channel_id, 25),
        );
        let mut cumulatives = [first.unwrap().cumulative, second.unwrap().cumulative];
        cumulatives.sort_unstable();
        assert_eq!(cumulatives, [25, 50], "each request is charged once");
        let state = store.inner.get_channel(&channel_id).await.unwrap().unwrap();
        assert_eq!(state.cumulative, 50);
    }

    #[tokio::test]
    async fn a_cancelled_settlement_reports_its_earnings_once() {
        let store = StallingStore::new(5, false);
        let (session, _, meter) = delegated_test_meter_on(
            vec![token_dimension(
                MeterDirection::Output,
                "/usage/completion_tokens",
            )],
            "cancelled-earnings",
            store,
        )
        .await;
        let reported = Arc::new(std::sync::Mutex::new(None));
        let sink = Arc::clone(&reported);
        let meter = meter.with_observer(Some(MeteredSettlement::with_callback(move |usd| {
            *sink.lock().unwrap() = Some(usd);
        })));
        let upstream = futures_util::stream::iter(vec![Ok::<_, std::io::Error>(usage_event(5))])
            .chain(futures_util::stream::pending());
        let mut metered = Box::pin(meter_delegated_response_stream(upstream, meter, true));
        assert!(
            tokio::time::timeout(Duration::from_millis(200), metered.next())
                .await
                .is_err()
        );
        drop(metered);

        let deadline = Instant::now() + Duration::from_secs(5);
        while reported.lock().unwrap().is_none() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let expected = 5.0 / 10f64.powi(i32::from(session.decimals()));
        let reported = reported.lock().unwrap().expect("earnings reported");
        assert!(
            (reported - expected).abs() < 1e-12,
            "{reported} != {expected}"
        );
    }

    #[tokio::test]
    async fn a_completed_stream_settles_provider_usage_once() {
        let (session, channel_id, meter) = interrupted_meter("completed-provider-usage").await;
        drain_delegated(
            vec![
                sse_text_event("hello world"),
                b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2}}\n\n"
                    .to_vec(),
                b"data: [DONE]\n\n".to_vec(),
            ],
            meter,
        )
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(session.committed_watermark(&channel_id), Some(5));
    }

    #[test]
    fn missing_output_usage_upper_bound_counts_utf8_bytes_only_at_finish() {
        let spec = SessionMeterSpec::new([SessionMeterDimension::required(
            MeterDirection::Output,
            BillingUnit::Tokens,
            1,
            1,
        )]);
        let mut accumulator = StreamUsageAccumulator::new(spec, SessionUsageHints::default());
        accumulator
            .observe_chunk(&sse_text_event("héllo wörld"), true)
            .unwrap();
        assert_eq!(
            accumulator
                .observation()
                .get(MeterDirection::Output, BillingUnit::Tokens),
            Some(2)
        );

        assert!(accumulator.finish_stream(true).unwrap());
        assert_eq!(
            accumulator
                .observation()
                .get(MeterDirection::Output, BillingUnit::Tokens),
            Some("héllo wörld".len() as u64)
        );
    }

    fn input_output_token_dimensions() -> Vec<MeterDimension> {
        vec![
            token_dimension(MeterDirection::Input, "/usage/prompt_tokens"),
            token_dimension(MeterDirection::Output, "/usage/completion_tokens"),
        ]
    }

    const OUTPUT_ONLY_USAGE: &[u8] =
        b"data: {\"choices\":[{\"delta\":{\"content\":\"one two three four five\"}}],\"usage\":{\"completion_tokens\":5}}\n\n";

    #[tokio::test]
    async fn missing_prompt_usage_settles_the_request_body_bound() {
        let (session, channel_id, meter) =
            delegated_test_meter(input_output_token_dimensions(), "missing-prompt-usage").await;

        drain_delegated(
            vec![OUTPUT_ONLY_USAGE.to_vec()],
            meter.with_request_body_bytes(600),
        )
        .await
        .unwrap();

        assert_eq!(
            session.committed_watermark(&channel_id),
            Some(605),
            "a missing prompt observation must not be rated as zero"
        );
    }

    #[tokio::test]
    async fn reported_prompt_usage_is_authoritative() {
        let (session, channel_id, meter) =
            delegated_test_meter(input_output_token_dimensions(), "reported-prompt-usage").await;

        drain_delegated(
            vec![
                b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":5}}\n\n"
                    .to_vec(),
            ],
            meter.with_request_body_bytes(600),
        )
        .await
        .unwrap();

        assert_eq!(session.committed_watermark(&channel_id), Some(105));
    }

    #[tokio::test]
    async fn missing_prompt_usage_without_a_body_bound_fails_the_stream() {
        let (_session, _channel_id, meter) =
            delegated_test_meter(input_output_token_dimensions(), "unbounded-prompt").await;

        let error = drain_delegated(vec![OUTPUT_ONLY_USAGE.to_vec()], meter)
            .await
            .expect_err("unrated input must not complete cleanly");

        assert!(
            error.to_string().contains("missing required observation"),
            "{error}"
        );
    }

    async fn client_voucher_stream(
        min_voucher_delta: u64,
        label: &str,
    ) -> (
        Arc<SessionMpp>,
        Arc<MemoryChannelStore>,
        String,
        SessionHandle,
        SessionStreamMeter,
    ) {
        let store_impl = Arc::new(MemoryChannelStore::new());
        let store: Arc<dyn ChannelStore> = store_impl.clone();
        let session = Arc::new(
            SessionMpp::new_with_channel_store(
                SessionConfig {
                    operator: solana_pubkey::Pubkey::new_unique().to_string(),
                    recipient: solana_pubkey::Pubkey::new_unique().to_string(),
                    suggested_deposit: Some(1_000),
                    currency: solana_pubkey::Pubkey::new_unique().to_string(),
                    network: "localnet".to_string(),
                    min_voucher_delta,
                    ..SessionConfig::default()
                },
                label,
                store,
            )
            .with_blockhash_cache(stream_test_blockhash_cache()),
        );
        let challenge = session.challenge(None).unwrap();
        let channel = solana_pubkey::Pubkey::new_unique();
        let signer = stream_test_signer();
        let authorized_signer = signer.pubkey().to_string();
        let handle = SessionHandle::new(channel, signer, challenge.clone());
        let channel_id = channel.to_string();
        store_impl
            .put_channel(
                &channel_id,
                test_channel_state(
                    &channel_id,
                    1_000,
                    &authorized_signer,
                    "client",
                    &challenge.id,
                    solana_pubkey::Pubkey::new_unique().to_string(),
                    None,
                ),
            )
            .await
            .unwrap();
        let context = SessionStreamContext::new(session.clone(), channel_id.clone(), 0);
        let spec = SessionMeterSpec::new([SessionMeterDimension::required(
            MeterDirection::Output,
            BillingUnit::Bytes,
            1,
            1,
        )]);
        let meter = SessionStreamMeter::new(spec, SessionUsageHints::default(), context).unwrap();
        (session, store_impl, channel_id, handle, meter)
    }

    #[tokio::test]
    async fn below_threshold_usage_is_not_released_before_its_commit() {
        let (session, store, channel_id, _handle, meter) =
            client_voucher_stream(10, "stream-drop-secret").await;
        let upstream =
            futures_util::stream::iter([Ok::<_, reqwest::Error>(Bytes::from_static(b"paid"))])
                .chain(futures_util::stream::pending());
        let mut metered = Box::pin(meter_response_stream(upstream, meter, false));

        assert!(
            tokio::time::timeout(Duration::from_millis(50), metered.next())
                .await
                .is_err(),
            "below-threshold usage must stay withheld until committed"
        );
        // The client disconnects: nothing billable was released unpaid.
        drop(metered);

        let state = store.get_channel(&channel_id).await.unwrap().unwrap();
        assert_eq!(state.cumulative, 0);
        assert_eq!(
            session.finalize_params(&channel_id).await.unwrap().settled,
            0
        );
    }

    #[tokio::test]
    async fn below_threshold_tail_is_released_after_final_commit() {
        let (session, _store, channel_id, handle, meter) =
            client_voucher_stream(10, "stream-tail-secret").await;
        let upstream = futures_util::stream::iter([
            Ok::<_, reqwest::Error>(Bytes::from_static(b"pa")),
            Ok::<_, reqwest::Error>(Bytes::from_static(b"id")),
        ]);
        let metered = meter_response_stream(upstream, meter, false);
        futures_util::pin_mut!(metered);

        assert!(
            tokio::time::timeout(Duration::from_millis(50), metered.next())
                .await
                .is_err()
        );
        // Vouchers below min_voucher_delta are rejected, so the client covers
        // the tail with one delta-sized commit.
        let voucher = handle.voucher_header(10).await.unwrap();
        session.process(&voucher).await.unwrap();

        let mut served = Vec::new();
        while let Some(chunk) = tokio::time::timeout(Duration::from_secs(1), metered.next())
            .await
            .unwrap()
        {
            served.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(served, b"paid");
        assert_eq!(session.committed_watermark(&channel_id), Some(10));
    }

    #[tokio::test]
    async fn committed_ahead_usage_streams_without_holding() {
        let (session, _store, _channel_id, handle, meter) =
            client_voucher_stream(10, "stream-ahead-secret").await;
        let voucher = handle.voucher_header(100).await.unwrap();
        session.process(&voucher).await.unwrap();
        let upstream =
            futures_util::stream::iter([Ok::<_, reqwest::Error>(Bytes::from_static(b"paid"))])
                .chain(futures_util::stream::pending());
        let metered = meter_response_stream(upstream, meter, false);
        futures_util::pin_mut!(metered);

        let released = tokio::time::timeout(Duration::from_secs(1), metered.next())
            .await
            .expect("prepaid usage must not be held")
            .unwrap()
            .unwrap();
        assert_eq!(released, Bytes::from_static(b"paid"));
    }
}
