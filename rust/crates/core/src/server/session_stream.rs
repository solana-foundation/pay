//! Session-aware streaming metering for proxied HTTP responses.
//!
//! The proxy uses this module to observe provider streams, rate the cumulative
//! usage through the YAML-derived session meter, and apply backpressure until
//! the client has committed a voucher for the newly delivered watermark.

use std::error::Error as StdError;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use futures_util::{Stream, StreamExt};
use pay_types::metering::{BillingUnit, MeterDimension, MeterDirection, Metering};
use serde_json::Value;

use crate::server::metering::RequestProperties;
use crate::server::session::SessionMpp;
use crate::server::session_metering::{
    GateMode, Result as MeteringResult, SessionGateDecision, SessionMeterDimension,
    SessionMeterSpec, SessionMeteringContext, SessionUsageGate, StablecoinSettlement,
    UsageObservation, spec_from_metering,
};

const COMMIT_WAIT_TIMEOUT: Duration = Duration::from_secs(30);
const COMMIT_POLL_INTERVAL: Duration = Duration::from_millis(50);

type BoxError = Box<dyn StdError + Send + Sync>;

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

    pub fn touch_channel(&self) {
        self.session_mpp.touch_channel(self.session_id.clone());
    }
}

#[derive(Debug, Clone, Default)]
pub struct SessionUsageHints {
    quota_units: Vec<QuotaUnitHint>,
}

#[derive(Debug, Clone, Copy)]
struct QuotaUnitHint {
    direction: MeterDirection,
    tokens_per_unit: u64,
}

impl SessionUsageHints {
    pub fn from_metering(metering: &Metering, variant_hint: Option<&str>) -> Self {
        let dimensions = select_meter_dimensions(metering, variant_hint).unwrap_or(&[]);
        let quota_units = dimensions
            .iter()
            .filter_map(|dimension| {
                if dimension.unit != BillingUnit::QuotaUnits {
                    return None;
                }
                let tokens_per_unit = dimension
                    .tiers
                    .iter()
                    .filter_map(|tier| tier.notes.as_deref())
                    .find_map(parse_tokens_per_quota_unit)?;
                Some(QuotaUnitHint {
                    direction: dimension.direction,
                    tokens_per_unit,
                })
            })
            .collect();

        Self { quota_units }
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

    let hints = SessionUsageHints::from_metering(metering, variant_hint);
    SessionStreamMeter::new(spec, hints, context).map(Some)
}

pub struct SessionStreamMeter {
    context: SessionStreamContext,
    gate: SessionUsageGate,
    accumulator: StreamUsageAccumulator,
    current: UsageObservation,
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
        })
    }

    pub fn observe_chunk(
        &mut self,
        chunk: &[u8],
        is_sse: bool,
    ) -> MeteringResult<Option<SessionGateDecision>> {
        if !self.accumulator.observe_chunk(chunk, is_sse) {
            return Ok(None);
        }

        self.current = self.accumulator.observation();
        self.gate
            .observe(self.current.clone(), GateMode::Streaming)
            .map(Some)
    }

    pub fn finish(&mut self) -> MeteringResult<Option<SessionGateDecision>> {
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

    fn touch_channel(&self) {
        self.context.touch_channel();
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
        while let Some(next) = stream.next().await {
            let chunk = next.map_err(box_error)?;
            meter.touch_channel();
            let decision = meter.observe_chunk(&chunk, is_sse).map_err(box_error)?;
            yield chunk;
            if let Some(decision) = decision {
                settle_decision(&mut meter, decision).await?;
            }
        }

        if let Some(decision) = meter.finish().map_err(box_error)? {
            settle_decision(&mut meter, decision).await?;
        }
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
    output_bytes: u64,
    output_chars: u64,
    output_words: u64,
    input_tokens: u64,
    output_tokens: u64,
    request_seen: bool,
    observed: bool,
}

impl StreamUsageAccumulator {
    fn new(spec: SessionMeterSpec, hints: SessionUsageHints) -> Self {
        Self {
            spec,
            hints,
            sse: SseUsageDecoder::default(),
            output_bytes: 0,
            output_chars: 0,
            output_words: 0,
            input_tokens: 0,
            output_tokens: 0,
            request_seen: false,
            observed: false,
        }
    }

    fn observe_chunk(&mut self, chunk: &[u8], is_sse: bool) -> bool {
        self.output_bytes = self.output_bytes.saturating_add(chunk.len() as u64);
        self.request_seen = true;

        let mut changed = false;
        if observes_unit(&self.spec, BillingUnit::Bytes) {
            changed = true;
        }

        if is_sse {
            changed |= self.observe_sse_chunk(chunk);
        } else if observes_unit(&self.spec, BillingUnit::Characters) {
            let text = String::from_utf8_lossy(chunk);
            self.output_chars = self
                .output_chars
                .saturating_add(text.chars().count() as u64);
            self.output_words = self.output_words.saturating_add(count_words(&text));
            changed = true;
        }

        self.observed |= changed;
        changed
    }

    fn observe_sse_chunk(&mut self, chunk: &[u8]) -> bool {
        let Ok(events) = self.sse.push_chunk(chunk) else {
            return false;
        };

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

            let text_chars = gemini_text_char_count(&value);
            if text_chars > 0 {
                self.output_chars = self.output_chars.saturating_add(text_chars);
                self.output_words = self
                    .output_words
                    .saturating_add(gemini_text_word_count(&value));
                changed = true;
            }

            if let Some(usage) = value.get("usageMetadata") {
                if let Some(input) = usage_u64(usage, "promptTokenCount") {
                    self.input_tokens = self.input_tokens.max(input);
                    changed = true;
                }

                let candidate_tokens = usage_u64(usage, "candidatesTokenCount").unwrap_or(0);
                let thought_tokens = usage_u64(usage, "thoughtsTokenCount").unwrap_or(0);
                let metered_output = usage_u64(usage, "totalTokenCount")
                    .and_then(|total| total.checked_sub(self.input_tokens))
                    .unwrap_or(0);
                let output = candidate_tokens
                    .saturating_add(thought_tokens)
                    .max(metered_output);
                if output > 0 {
                    self.output_tokens = self.output_tokens.max(output);
                    changed = true;
                }
            }
        }

        changed
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
            (MeterDirection::Input, BillingUnit::Tokens) => self.input_tokens,
            (MeterDirection::Output, BillingUnit::Tokens) => {
                self.output_tokens.max(self.output_words)
            }
            (MeterDirection::Input, BillingUnit::QuotaUnits) => self
                .hints
                .tokens_per_quota_unit(MeterDirection::Input)
                .map(|scale| ceil_div_u64(self.input_tokens, scale))
                .unwrap_or(self.input_tokens),
            (MeterDirection::Output, BillingUnit::QuotaUnits) => {
                let output_tokens = self.output_tokens.max(self.output_words);
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
}

#[derive(Debug, Clone, Default)]
struct SseUsageDecoder {
    buffer: String,
}

#[derive(Debug, Clone)]
struct SseUsageEvent {
    data: Option<String>,
}

impl SseUsageDecoder {
    fn push_chunk(&mut self, chunk: &[u8]) -> Result<Vec<SseUsageEvent>, std::str::Utf8Error> {
        let text = std::str::from_utf8(chunk)?;
        self.buffer
            .push_str(&text.replace("\r\n", "\n").replace('\r', "\n"));

        let mut events = vec![];
        while let Some(index) = self.buffer.find("\n\n") {
            let block = self.buffer[..index].to_string();
            self.buffer.drain(..index + 2);
            events.push(parse_sse_event(&block));
        }
        Ok(events)
    }
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
    spec.dimensions.iter().any(|dimension| {
        matches!(
            (dimension.direction, dimension.unit),
            (MeterDirection::Output, BillingUnit::Bytes)
                | (MeterDirection::Output, BillingUnit::Characters)
                | (MeterDirection::Output, BillingUnit::Tokens)
                | (MeterDirection::Output, BillingUnit::QuotaUnits)
        )
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
        if let Some(hint) = variant_hint
            && let Some(variant) = metering
                .variants
                .iter()
                .find(|variant| hint.contains(&variant.value))
        {
            return Some(&variant.dimensions);
        }
        return metering
            .variants
            .first()
            .map(|variant| variant.dimensions.as_slice());
    }

    (!metering.dimensions.is_empty()).then_some(&metering.dimensions)
}

fn parse_tokens_per_quota_unit(notes: &str) -> Option<u64> {
    let lower = notes.to_ascii_lowercase();
    let marker = " tokens per quota unit";
    let index = lower.find(marker)?;
    let prefix = lower[..index].trim_end();
    prefix.split_whitespace().find_map(|part| {
        part.replace(',', "")
            .parse::<u64>()
            .ok()
            .filter(|value| *value > 0)
    })
}

fn gemini_text_char_count(value: &Value) -> u64 {
    gemini_texts(value)
        .map(|text| text.chars().count() as u64)
        .sum()
}

fn gemini_text_word_count(value: &Value) -> u64 {
    gemini_texts(value).map(count_words).sum()
}

fn gemini_texts(value: &Value) -> impl Iterator<Item = &str> {
    value
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
        .filter_map(|part| part.get("text").and_then(Value::as_str))
}

fn usage_u64(value: &Value, key: &str) -> Option<u64> {
    value.get(key).and_then(Value::as_u64)
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
        }
    }

    fn metering(dimensions: Vec<MeterDimension>) -> Metering {
        Metering {
            dimensions,
            variants: vec![],
            sku_tiers: vec![],
            splits: vec![],
        }
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
        );
        let mut accumulator = StreamUsageAccumulator::new(spec, hints);

        let changed = accumulator.observe_chunk(
            br#"data: {"usageMetadata":{"promptTokenCount":27,"candidatesTokenCount":448}}

"#,
            true,
        );
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
        );
        let mut accumulator = StreamUsageAccumulator::new(spec, hints);

        let changed = accumulator.observe_chunk(
            br#"data: {"candidates":[{"content":{"parts":[{"text":"one two three four"}]}}]}

"#,
            true,
        );

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
}
