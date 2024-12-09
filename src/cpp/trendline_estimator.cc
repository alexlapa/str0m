/*
 *  Copyright (c) 2016 The WebRTC project authors. All Rights Reserved.
 *
 *  Use of this source code is governed by a BSD-style license
 *  that can be found in the LICENSE file in the root of the source
 *  tree. An additional intellectual property rights grant can be found
 *  in the file PATENTS.  All contributing project authors may
 *  be found in the AUTHORS file in the root of the source tree.
 */

#include "trendline_estimator.h"

#include <math.h>

#include <algorithm>
#include <cstddef>
#include <cstdint>
#include <cstdio>
#include <deque>
#include <memory>
#include <optional>
#include <string>
#include <utility>

#include "network_state_predictor.h"
#include "bandwidth_usage.h"

namespace webrtc {

namespace {

// Parameters for linear least squares fit of regression line to noisy data.
constexpr double kDefaultTrendlineSmoothingCoeff = 0.9;
constexpr double kDefaultTrendlineThresholdGain = 4.0;
const char kBweWindowSizeInPacketsExperiment[] = "WebRTC-BweWindowSizeInPackets";

std::optional<double> LinearFitSlope(const std::deque<TrendlineEstimator::PacketTiming>& packets) {
  // Compute the "center of mass".
  double sum_x = 0;
  double sum_y = 0;
  for (const auto& packet : packets) {
    sum_x += packet.arrival_time_ms;
    sum_y += packet.smoothed_delay_ms;
  }
  double x_avg = sum_x / packets.size();
  double y_avg = sum_y / packets.size();
  // Compute the slope k = \sum (x_i-x_avg)(y_i-y_avg) / \sum (x_i-x_avg)^2
  double numerator = 0;
  double denominator = 0;
  for (const auto& packet : packets) {
    double x = packet.arrival_time_ms;
    double y = packet.smoothed_delay_ms;
    numerator += (x - x_avg) * (y - y_avg);
    denominator += (x - x_avg) * (x - x_avg);
  }
  if (denominator == 0)
    return std::nullopt;
  return numerator / denominator;
}

std::optional<double> ComputeSlopeCap(
    const std::deque<TrendlineEstimator::PacketTiming>& packets,
    const TrendlineEstimatorSettings& settings) {
  TrendlineEstimator::PacketTiming early = packets[0];
  for (size_t i = 1; i < settings.beginning_packets; ++i) {
    if (packets[i].raw_delay_ms < early.raw_delay_ms)
      early = packets[i];
  }
  size_t late_start = packets.size() - settings.end_packets;
  TrendlineEstimator::PacketTiming late = packets[late_start];
  for (size_t i = late_start + 1; i < packets.size(); ++i) {
    if (packets[i].raw_delay_ms < late.raw_delay_ms)
      late = packets[i];
  }
  if (late.arrival_time_ms - early.arrival_time_ms < 1) {
    return std::nullopt;
  }
  return (late.raw_delay_ms - early.raw_delay_ms) /
             (late.arrival_time_ms - early.arrival_time_ms) +
         settings.cap_uncertainty;
}

constexpr double kMaxAdaptOffsetMs = 15.0;
constexpr double kOverUsingTimeThreshold = 10;
constexpr int kMinNumDeltas = 60;
constexpr int kDeltaCounterMax = 1000;

}  // namespace

constexpr char TrendlineEstimatorSettings::kKey[];

TrendlineEstimatorSettings::TrendlineEstimatorSettings() {
  // if (absl::StartsWith(key_value_config->Lookup(kBweWindowSizeInPacketsExperiment), "Enabled")) {
    window_size = TrendlineEstimatorSettings::kDefaultTrendlineWindowSize;
  // }
  // Parser()->Parse(key_value_config->Lookup(TrendlineEstimatorSettings::kKey));
  if (window_size < 10 || 200 < window_size) {
    window_size = kDefaultTrendlineWindowSize;
  }
  if (enable_cap) {
    if (beginning_packets < 1 || end_packets < 1 ||
        beginning_packets > window_size || end_packets > window_size) {
      enable_cap = false;
      beginning_packets = end_packets = 0;
      cap_uncertainty = 0.0;
    }
    if (beginning_packets + end_packets > window_size) {
      enable_cap = false;
      beginning_packets = end_packets = 0;
      cap_uncertainty = 0.0;
    }
    if (cap_uncertainty < 0.0 || 0.025 < cap_uncertainty) {
      cap_uncertainty = 0.0;
    }
  }
}

TrendlineEstimator::TrendlineEstimator(
    NetworkStatePredictor* network_state_predictor)
    : settings_(),
      smoothing_coef_(kDefaultTrendlineSmoothingCoeff),
      threshold_gain_(kDefaultTrendlineThresholdGain),
      num_of_deltas_(0),
      first_arrival_time_ms_(-1),
      accumulated_delay_(0),
      smoothed_delay_(0),
      delay_hist_(),
      k_up_(0.0087),
      k_down_(0.039),
      overusing_time_threshold_(kOverUsingTimeThreshold),
      threshold_(12.5),
      prev_modified_trend_(NAN),
      last_update_ms_(-1),
      prev_trend_(0.0),
      time_over_using_(-1),
      overuse_counter_(0),
      hypothesis_(BandwidthUsage::kBwNormal),
      hypothesis_predicted_(BandwidthUsage::kBwNormal),
      network_state_predictor_(network_state_predictor) {
}

TrendlineEstimator::~TrendlineEstimator() {}

void TrendlineEstimator::UpdateTrendline(double recv_delta_ms,
                                         double send_delta_ms,
                                         int64_t arrival_time_ms) {
  const double delta_ms = recv_delta_ms - send_delta_ms;
  ++num_of_deltas_;
  num_of_deltas_ = std::min(num_of_deltas_, kDeltaCounterMax);
  if (first_arrival_time_ms_ == -1)
    first_arrival_time_ms_ = arrival_time_ms;

  // Exponential backoff filter.
  accumulated_delay_ += delta_ms;
  smoothed_delay_ = smoothing_coef_ * smoothed_delay_ +
                    (1 - smoothing_coef_) * accumulated_delay_;

  // Maintain packet window
  delay_hist_.emplace_back(
      static_cast<double>(arrival_time_ms - first_arrival_time_ms_),
      smoothed_delay_, accumulated_delay_);
  if (settings_.enable_sort) {
    for (size_t i = delay_hist_.size() - 1;
         i > 0 &&
         delay_hist_[i].arrival_time_ms < delay_hist_[i - 1].arrival_time_ms;
         --i) {
      std::swap(delay_hist_[i], delay_hist_[i - 1]);
    }
  }
  if (delay_hist_.size() > settings_.window_size)
    delay_hist_.pop_front();

  // Simple linear regression.
  double trend = prev_trend_;
  if (delay_hist_.size() == settings_.window_size) {
    // Update trend_ if it is possible to fit a line to the data. The delay
    // trend can be seen as an estimate of (send_rate - capacity)/capacity.
    // 0 < trend < 1   ->  the delay increases, queues are filling up
    //   trend == 0    ->  the delay does not change
    //   trend < 0     ->  the delay decreases, queues are being emptied
    trend = LinearFitSlope(delay_hist_).value_or(trend);
    if (settings_.enable_cap) {
      std::optional<double> cap = ComputeSlopeCap(delay_hist_, settings_);
      // We only use the cap to filter out overuse detections, not
      // to detect additional underuses.
      if (trend >= 0 && cap.has_value() && trend > cap.value()) {
        trend = cap.value();
      }
    }
  }

  Detect(trend, send_delta_ms, arrival_time_ms);
}

void TrendlineEstimator::Update(double recv_delta_ms,
                               double send_delta_ms,
                               int64_t arrival_time_ms) {
  UpdateTrendline(recv_delta_ms, send_delta_ms, arrival_time_ms);

//  if (network_state_predictor_) {
//     hypothesis_predicted_ = network_state_predictor_->Update(
//         send_time_ms, arrival_time_ms, hypothesis_);
//  }
}

BandwidthUsage TrendlineEstimator::State() const {
  return network_state_predictor_ ? hypothesis_predicted_ : hypothesis_;
}

int TrendlineEstimator::num_of_deltas() const {
    return num_of_deltas_;
};
double TrendlineEstimator::accumulated_delay() const {
    return accumulated_delay_;
};
double TrendlineEstimator::smoothed_delay() const {
    return smoothed_delay_;
};

void TrendlineEstimator::Detect(double trend, double ts_delta, int64_t now_ms) {
  if (num_of_deltas_ < 2) {
    hypothesis_ = BandwidthUsage::kBwNormal;
    return;
  }
  const double modified_trend =
      std::min(num_of_deltas_, kMinNumDeltas) * trend * threshold_gain_;
  prev_modified_trend_ = modified_trend;
  if (modified_trend > threshold_) {
    if (time_over_using_ == -1) {
      // Initialize the timer. Assume that we've been
      // over-using half of the time since the previous
      // sample.
      time_over_using_ = ts_delta / 2;
    } else {
      // Increment timer
      time_over_using_ += ts_delta;
    }
    overuse_counter_++;
    if (time_over_using_ > overusing_time_threshold_ && overuse_counter_ > 1) {
      if (trend >= prev_trend_) {
        time_over_using_ = 0;
        overuse_counter_ = 0;
        hypothesis_ = BandwidthUsage::kBwOverusing;
      }
    }
  } else if (modified_trend < -threshold_) {
    time_over_using_ = -1;
    overuse_counter_ = 0;
    hypothesis_ = BandwidthUsage::kBwUnderusing;
  } else {
    time_over_using_ = -1;
    overuse_counter_ = 0;
    hypothesis_ = BandwidthUsage::kBwNormal;
  }
  prev_trend_ = trend;
  UpdateThreshold(modified_trend, now_ms);
}

void TrendlineEstimator::UpdateThreshold(double modified_trend,
                                         int64_t now_ms) {
  if (last_update_ms_ == -1)
    last_update_ms_ = now_ms;

  if (fabs(modified_trend) > threshold_ + kMaxAdaptOffsetMs) {
    // Avoid adapting the threshold to big latency spikes, caused e.g.,
    // by a sudden capacity drop.
    last_update_ms_ = now_ms;
    return;
  }

  const double k = fabs(modified_trend) < threshold_ ? k_down_ : k_up_;
  const int64_t kMaxTimeDeltaMs = 100;
  int64_t time_delta_ms = std::min(now_ms - last_update_ms_, kMaxTimeDeltaMs);
  threshold_ += k * (fabs(modified_trend) - threshold_) * time_delta_ms;
  // threshold_ = rtc::SafeClamp(threshold_, 6.f, 600.f);
  threshold_ = std::clamp(threshold_, 6.0, 600.0);
  last_update_ms_ = now_ms;
}
}  // namespace webrtc