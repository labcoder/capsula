use std::fmt;

use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{self, SeqAccess, Visitor},
    ser::SerializeTuple,
};

const FIELDS: usize = 99;

/// A fixed drawing-font snapshot in upward-rounded units of 1/1024 em.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextMetricsV1 {
    /// Printable ASCII advances in code-point order, from space to tilde.
    pub advances: [u16; 95],
    /// The maximum of the regular and bold line heights over the sampled sizes.
    pub line_height: u16,
    /// The CJK/fullwidth, remaining BMP and wide-exception/supplementary maxima, in that order.
    pub range_maxima: [u16; 3],
}

impl TextMetricsV1 {
    /// Whether the whole snapshot is usable; one invalid field selects the built-in measure.
    pub fn valid(&self) -> bool {
        self.advances
            .iter()
            .all(|value| (64..=8192).contains(value))
            && (512..=4096).contains(&self.line_height)
            && self
                .range_maxima
                .into_iter()
                .zip([1024, 1536, 2048])
                .all(|(value, floor)| (floor..=8192).contains(&value))
    }
}

impl Serialize for TextMetricsV1 {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut tuple = serializer.serialize_tuple(FIELDS)?;
        for value in self
            .advances
            .iter()
            .chain(std::iter::once(&self.line_height))
            .chain(&self.range_maxima)
        {
            tuple.serialize_element(value)?;
        }
        tuple.end()
    }
}

impl<'de> Deserialize<'de> for TextMetricsV1 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Metrics;

        impl<'de> Visitor<'de> for Metrics {
            type Value = TextMetricsV1;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("99 font metrics")
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut snapshot = TextMetricsV1 {
                    advances: [0; 95],
                    line_height: 0,
                    range_maxima: [0; 3],
                };
                for (index, value) in snapshot
                    .advances
                    .iter_mut()
                    .chain(std::iter::once(&mut snapshot.line_height))
                    .chain(&mut snapshot.range_maxima)
                    .enumerate()
                {
                    *value = seq
                        .next_element()?
                        .ok_or_else(|| de::Error::invalid_length(index, &self))?;
                }
                Ok(snapshot)
            }
        }

        deserializer.deserialize_tuple(FIELDS, Metrics)
    }
}

#[cfg(test)]
mod tests {
    use super::TextMetricsV1;

    #[test]
    fn all_fields_accept_inclusive_limits_and_reject_neighbors() {
        let floor = TextMetricsV1 {
            advances: [64; 95],
            line_height: 512,
            range_maxima: [1024, 1536, 2048],
        };
        let ceiling = TextMetricsV1 {
            advances: [8192; 95],
            line_height: 4096,
            range_maxima: [8192; 3],
        };
        assert!(floor.valid());
        assert!(ceiling.valid());
        for (mut metrics, step) in [(floor, -1), (ceiling, 1)] {
            let advances = metrics.advances[94];
            metrics.advances[94] = advances.checked_add_signed(step).unwrap();
            assert!(!metrics.valid());
            metrics.advances[94] = advances;
            let line = metrics.line_height;
            metrics.line_height = line.checked_add_signed(step).unwrap();
            assert!(!metrics.valid());
            metrics.line_height = line;
            for i in 0..3 {
                let width = metrics.range_maxima[i];
                metrics.range_maxima[i] = width.checked_add_signed(step).unwrap();
                assert!(!metrics.valid());
                metrics.range_maxima[i] = width;
            }
        }
    }
}
