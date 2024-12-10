use std::collections::VecDeque;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use crate::rtp_::Bitrate;
use crate::util::already_happened;

use super::arrival_group::ArrivalGroupAccumulator;
use super::rate_control::RateControl;
use super::trendline_estimator::TrendlineEstimator;
use super::{AckedPacket, BandwidthUsage};

const MAX_RTT_HISTORY_WINDOW: usize = 32;
const UPDATE_INTERVAL: Duration = Duration::from_millis(25);
/// The maximum time we keep updating our estimate without receiving a TWCC report.
const MAX_TWCC_GAP: Duration = Duration::from_millis(500);

/// Delay controller for googcc inspired BWE.
///
/// This controller attempts to estimate the available send bandwidth by looking at the variations
/// in packet arrival times for groups of packets sent together. Broadly, if the delay variation is
/// increasing this indicates overuse.
pub struct DelayController {
    arrival_group_accumulator: ArrivalGroupAccumulator,
    trendline_estimator: TrendlineEstimator,
    started_at: Instant,
    remote_started_at: Option<Instant>,
    cpp_trendline_estimator: std::sync::Mutex<cxx::UniquePtr<crate::bridge::TrendlineEstimator>>,
    cpp_arrival_group_accumulator:
        std::sync::Mutex<cxx::UniquePtr<crate::bridge::InterArrivalDelta>>,

    rate_control: RateControl,
    /// Last estimate produced, unlike [`next_estimate`] this will always have a value after the
    /// first estimate.
    last_estimate: Option<Bitrate>,
    /// History of the max RTT derived for each TWCC report.
    max_rtt_history: VecDeque<Duration>,
    /// Calculated mean of max_rtt_history.
    mean_max_rtt: Option<Duration>,

    /// The next time we should poll.
    next_timeout: Instant,
    /// The last time we ingested a TWCC report.
    last_twcc_report: Instant,
}

impl DelayController {
    pub fn new(initial_bitrate: Bitrate) -> Self {
        Self {
            arrival_group_accumulator: ArrivalGroupAccumulator::default(),
            trendline_estimator: TrendlineEstimator::new(20),
            started_at: Instant::now(),
            remote_started_at: None,
            cpp_trendline_estimator: std::sync::Mutex::new(crate::bridge::new_trendline_estimator()),
            cpp_arrival_group_accumulator: std::sync::Mutex::new(
                crate::bridge::new_inter_arrival_delta(),
            ),
            rate_control: RateControl::new(initial_bitrate, Bitrate::kbps(40), Bitrate::gbps(10)),
            last_estimate: None,
            max_rtt_history: VecDeque::default(),
            mean_max_rtt: None,
            next_timeout: already_happened(),
            last_twcc_report: already_happened(),
        }
    }

    /// Record a packet from a TWCC report.
    pub(crate) fn update(
        &mut self,
        acked: &[AckedPacket],
        acked_bitrate: Option<Bitrate>,
        now: Instant,
        from: SocketAddr,
    ) -> Option<Bitrate> {
        let mut max_rtt = None;

        for acked_packet in acked {
            self.remote_started_at.get_or_insert(acked_packet.remote_recv_time);

            max_rtt = max_rtt.max(Some(acked_packet.rtt()));
            if let Some(dv) = self
                .arrival_group_accumulator
                .accumulate_packet(acked_packet)
            {
                // Got a new delay variation, add it to the trendline
                self.trendline_estimator
                    .add_delay_observation(dv, now);
            }
        }
        let old_hyp = self.trendline_estimator.hypothesis();

        for acked_packet in acked {
            self.remote_started_at.get_or_insert(acked_packet.remote_recv_time);

            let send_time_us = (acked_packet.local_send_time - self.started_at).as_micros() as u64;
            let arrival_time_us = (acked_packet.remote_recv_time - self.remote_started_at.unwrap())
                .as_micros() as u64;
            let arrival_time_ms = Duration::from_micros(arrival_time_us).as_millis() as i64;
            let system_time_us = (now - self.started_at).as_micros() as u64;
            let packet_size = acked_packet.size.as_bytes_usize() as u64;
            let mut send_delta_us = 0u64;
            let mut recv_delta_us = 0u64;
            let mut packet_size_delta = 0u64;
            let calculated_deltas = crate::bridge::ComputeDeltas(
                self.cpp_arrival_group_accumulator
                    .lock()
                    .unwrap()
                    .as_mut()
                    .unwrap(),
                send_time_us,
                arrival_time_us,
                system_time_us,
                packet_size,
                &mut send_delta_us,
                &mut recv_delta_us,
                &mut packet_size_delta,
            );
            let send_delta = Duration::from_micros(send_delta_us);
            let recv_delta = Duration::from_micros(recv_delta_us);

            if calculated_deltas {
                self.cpp_trendline_estimator
                    .lock()
                    .unwrap()
                    .as_mut()
                    .unwrap()
                    .Update(
                        send_delta.as_secs_f64() * 1000.0,
                        recv_delta.as_secs_f64() * 1000.0,
                        arrival_time_ms,
                    )
            }
        }

        if let Some(rtt) = max_rtt {
            self.add_max_rtt(rtt);
        }

        let new_hypothesis: BandwidthUsage = self.cpp_trendline_estimator.lock().unwrap().as_mut().unwrap().State().into();

        self.update_estimate(new_hypothesis, acked_bitrate, self.mean_max_rtt, now);
        self.last_twcc_report = now;

        error!(
            "From [{from}], hypothesis = {old_hyp:?} => {new_hypothesis:?}, estimate = {:?}",
            self.last_estimate,
        );

        self.last_estimate
    }

    pub(crate) fn poll_timeout(&self) -> Instant {
        self.next_timeout
    }

    pub(crate) fn handle_timeout(&mut self, acked_bitrate: Option<Bitrate>, now: Instant) {
        if !self.trendline_hypothesis_valid(now) {
            // We haven't received a TWCC report in a while. The trendline hypothesis can
            // no longer be considered valid. We need another TWCC report before we can update
            // estimates.
            let next_timeout_in = self
                .mean_max_rtt
                .unwrap_or(MAX_TWCC_GAP)
                .min(UPDATE_INTERVAL);

            // Set this even if we didn't update, otherwise we get stuck in a poll -> handle loop
            // that starves the run loop.
            self.next_timeout = now + next_timeout_in;
            return;
        }

        self.update_estimate(
            self.trendline_estimator.hypothesis(),
            acked_bitrate,
            self.mean_max_rtt,
            now,
        );
    }

    /// Get the latest estimate.
    pub(crate) fn last_estimate(&self) -> Option<Bitrate> {
        self.last_estimate
    }

    fn add_max_rtt(&mut self, max_rtt: Duration) {
        while self.max_rtt_history.len() > MAX_RTT_HISTORY_WINDOW {
            self.max_rtt_history.pop_front();
        }
        self.max_rtt_history.push_back(max_rtt);

        let sum = self
            .max_rtt_history
            .iter()
            .fold(Duration::ZERO, |acc, rtt| acc + *rtt);

        self.mean_max_rtt = Some(sum / self.max_rtt_history.len() as u32);
    }

    fn update_estimate(
        &mut self,
        hypothesis: BandwidthUsage,
        observed_bitrate: Option<Bitrate>,
        mean_max_rtt: Option<Duration>,
        now: Instant,
    ) {
        if let Some(observed_bitrate) = observed_bitrate {
            self.rate_control
                .update(hypothesis.into(), observed_bitrate, mean_max_rtt, now);
            let estimated_rate = self.rate_control.estimated_bitrate();

            crate::packet::bwe::macros::log_bitrate_estimate!(estimated_rate.as_f64());
            self.last_estimate = Some(estimated_rate);
        }

        // Set this even if we didn't update, otherwise we get stuck in a poll -> handle loop
        // that starves the run loop.
        self.next_timeout = now + UPDATE_INTERVAL;
    }

    /// Whether the current trendline hypothesis is valid i.e. not too old.
    fn trendline_hypothesis_valid(&self, now: Instant) -> bool {
        now.duration_since(self.last_twcc_report)
            <= self
                .mean_max_rtt
                .map(|rtt| rtt * 2)
                .unwrap_or(MAX_TWCC_GAP)
                .min(UPDATE_INTERVAL * 2)
    }
}
