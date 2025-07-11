// Copyright 2025 OpenObserve Inc.
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

use datafusion::error::Result;

use crate::service::promql::value::{RangeValue, Value};

pub(crate) fn irate(data: Value) -> Result<Value> {
    super::eval_idelta(data, "irate", exec, false)
}

fn exec(data: RangeValue) -> Option<f64> {
    if data.samples.len() < 2 {
        return None;
    }
    let last = data.samples.last().unwrap();
    let previous = data.samples.get(data.samples.len() - 2).unwrap();
    let dt_seconds = (last.timestamp - previous.timestamp) as f64 / 1_000_000.0;
    if dt_seconds == 0.0 {
        return Some(0.0);
    }
    let dt_value = if last.value - previous.value >= 0.0 {
        last.value - previous.value
    } else {
        last.value
    };
    Some(dt_value / dt_seconds)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::service::promql::value::{Labels, RangeValue, TimeWindow};

    #[test]
    fn test_irate_function() {
        // Create a range value with increasing counter values
        let samples = vec![
            crate::service::promql::value::Sample::new(1000, 10.0),
            crate::service::promql::value::Sample::new(2000, 20.0),
            crate::service::promql::value::Sample::new(3000, 30.0),
        ];

        let range_value = RangeValue {
            labels: Labels::default(),
            samples,
            exemplars: None,
            time_window: Some(TimeWindow {
                eval_ts: 3000,
                range: Duration::from_secs(2),
                offset: Duration::ZERO,
            }),
        };

        let matrix = Value::Matrix(vec![range_value]);
        let result = irate(matrix).unwrap();

        // Should return a vector with irate value
        match result {
            Value::Vector(v) => {
                assert_eq!(v.len(), 1);
                // Irate should be positive for increasing counter
                assert!(v[0].sample.value > 0.0);
                assert_eq!(v[0].sample.timestamp, 3000);
            }
            _ => panic!("Expected Vector result"),
        }
    }
}
