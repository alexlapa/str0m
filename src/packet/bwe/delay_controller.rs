use std::collections::VecDeque;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use crate::rtp_::Bitrate;
use crate::util::already_happened;

use super::arrival_group2::ArrivalGroupAccumulator as ArrivalGroupAccumulator2;
use super::rate_control::RateControl;
use super::trendline_estimator2::TrendlineEstimator as TrendlineEstimator2;
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
    rate_control: RateControl,
    last_estimate: Option<Bitrate>,

    cpp_trendline_estimator: std::sync::Mutex<cxx::UniquePtr<crate::bridge::TrendlineEstimator>>,
    cpp_arrival_group_accumulator:
        std::sync::Mutex<cxx::UniquePtr<crate::bridge::InterArrivalDelta>>,
    cpp_rate_control: RateControl,
    cpp_last_estimate: Option<Bitrate>,

    arrival_group_accumulator2: ArrivalGroupAccumulator2,
    trendline_estimator2: std::sync::Mutex<cxx::UniquePtr<crate::bridge::TrendlineEstimator>>,
    rate_control2: RateControl,
    last_estimate2: Option<Bitrate>,

    started_at: Instant,

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
            started_at: Instant::now(),
            cpp_trendline_estimator: std::sync::Mutex::new(crate::bridge::new_trendline_estimator()),
            cpp_arrival_group_accumulator: std::sync::Mutex::new(
                crate::bridge::new_inter_arrival_delta(),
            ),
            cpp_rate_control: RateControl::new(
                initial_bitrate,
                Bitrate::kbps(40),
                Bitrate::gbps(10),
            ),
            cpp_last_estimate: None,
            arrival_group_accumulator2: ArrivalGroupAccumulator2::default(),
            trendline_estimator2: std::sync::Mutex::new(crate::bridge::new_trendline_estimator()),
            rate_control: RateControl::new(initial_bitrate, Bitrate::kbps(40), Bitrate::gbps(10)),
            last_estimate: None,
            max_rtt_history: VecDeque::default(),
            mean_max_rtt: None,
            next_timeout: already_happened(),
            last_twcc_report: already_happened(),
            rate_control2: RateControl::new(initial_bitrate, Bitrate::kbps(40), Bitrate::gbps(10)),
            last_estimate2: None,
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

        // old hyp
        // for acked_packet in acked {
        //     max_rtt = max_rtt.max(Some(acked_packet.rtt()));
        //     if let Some(dv) = self
        //         .arrival_group_accumulator
        //         .accumulate_packet(acked_packet)
        //     {
        //         // Got a new delay variation, add it to the trendline
        //         self.trendline_estimator.add_delay_observation(dv, now);
        //     }
        // }

        // new hyp
        for acked_packet in acked {
            let arrival_time_ms = acked_packet.remote_recv_time_ms as u64;
            if let Some(dv) = self.arrival_group_accumulator2.compute_deltas(acked_packet) {
                let send_time_delta = dv.send_time_delta * 1000.0;
                let arrival_time_delta = dv.arrival_time_delta * 1000.0;
                let arrival_time_ms = arrival_time_ms as i64;
                self.trendline_estimator2
                    .lock()
                    .unwrap()
                    .as_mut()
                    .unwrap()
                    .Update(
                        arrival_time_delta,
                        send_time_delta,
                        arrival_time_ms,
                    );
            }
        }

        // cpp hyp
        for acked_packet in acked {
            let send_time_us = acked_packet.local_send_time_ms as u64 * 1000;
            let arrival_time_us = acked_packet.remote_recv_time_ms as u64 * 1000;
            let system_time_us = (now - self.started_at).as_millis() as u64 * 1000;
            let packet_size = acked_packet.size.as_bytes_usize() as u64;
            let mut send_delta_us = 0i64;
            let mut recv_delta_us = 0i64;
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

            if calculated_deltas {
                let send_time_delta = send_delta_us as f64 / 1000.0;
                let arrival_time_delta = recv_delta_us as f64 / 1000.0;
                let arrival_time_ms = Duration::from_micros(arrival_time_us).as_millis() as i64;

                self.cpp_trendline_estimator
                    .lock()
                    .unwrap()
                    .as_mut()
                    .unwrap()
                    .Update(
                        arrival_time_delta,
                        send_time_delta,
                        arrival_time_ms,
                    )
            }
        }

        if let Some(rtt) = max_rtt {
            self.add_max_rtt(rtt);
        }

        let new_hyp: BandwidthUsage = self.trendline_estimator2.lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .State()
            .into();
        let cpp_hyp: BandwidthUsage = self
            .cpp_trendline_estimator
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .State()
            .into();

        self.update_estimate_new(new_hyp, acked_bitrate, self.mean_max_rtt, now);
        self.update_estimate_cpp(cpp_hyp, acked_bitrate, self.mean_max_rtt, now);

        self.last_twcc_report = now;

        error!(
            "From [{from}], acked = {acked_bitrate:?}, NEW: {new_hyp} => {}, CPP: {cpp_hyp} => {}",
            self.last_estimate2.unwrap_or(Bitrate::ZERO).as_u64(),
            self.cpp_last_estimate.unwrap_or(Bitrate::ZERO).as_u64()
        );

        self.last_estimate2
    }

    pub(crate) fn poll_timeout(&self) -> Instant {
        self.next_timeout
    }

    pub(crate) fn handle_timeout(&mut self, acked_bitrate: Option<Bitrate>, now: Instant) {
        // if !self.trendline_hypothesis_valid(now) {
        //     // We haven't received a TWCC report in a while. The trendline hypothesis can
        //     // no longer be considered valid. We need another TWCC report before we can update
        //     // estimates.
        //     let next_timeout_in = self
        //         .mean_max_rtt
        //         .unwrap_or(MAX_TWCC_GAP)
        //         .min(UPDATE_INTERVAL);
        //
        //     // Set this even if we didn't update, otherwise we get stuck in a poll -> handle loop
        //     // that starves the run loop.
        //     self.next_timeout = now + next_timeout_in;
        //     return;
        // }
        //
        // self.update_estimate_new(
        //     self.trendline_estimator2.hypothesis(),
        //     acked_bitrate,
        //     self.mean_max_rtt,
        //     now,
        // );
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

    fn update_estimate_old(
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

    fn update_estimate_new(
        &mut self,
        hypothesis: BandwidthUsage,
        observed_bitrate: Option<Bitrate>,
        mean_max_rtt: Option<Duration>,
        now: Instant,
    ) {
        if let Some(observed_bitrate) = observed_bitrate {
            self.rate_control2
                .update(hypothesis.into(), observed_bitrate, mean_max_rtt, now);
            let estimated_rate = self.rate_control2.estimated_bitrate();

            crate::packet::bwe::macros::log_bitrate_estimate!(estimated_rate.as_f64());
            self.last_estimate2 = Some(estimated_rate);
        }

        // Set this even if we didn't update, otherwise we get stuck in a poll -> handle loop
        // that starves the run loop.
        self.next_timeout = now + UPDATE_INTERVAL;
    }

    fn update_estimate_cpp(
        &mut self,
        hypothesis: BandwidthUsage,
        observed_bitrate: Option<Bitrate>,
        mean_max_rtt: Option<Duration>,
        now: Instant,
    ) {
        if let Some(observed_bitrate) = observed_bitrate {
            self.cpp_rate_control
                .update(hypothesis.into(), observed_bitrate, mean_max_rtt, now);
            let estimated_rate = self.cpp_rate_control.estimated_bitrate();

            crate::packet::bwe::macros::log_bitrate_estimate!(estimated_rate.as_f64());
            self.cpp_last_estimate = Some(estimated_rate);
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
